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
//!
//! v7.8 (plan N-01/N-02/N-03 — "0 silent drops"):
//! - Directories are pruned by name only when the name is unambiguous
//!   (`.git`, `node_modules`, `__pycache__`, …). `target/` needs a sibling
//!   `Cargo.toml`, virtualenvs need `pyvenv.cfg`/`conda-meta`, caches need
//!   `CACHEDIR.TAG`. `packages/`, `build/`, `debug/`, `release/`, `env/`,
//!   `vendor/`, `coverage/`… are source in plenty of repos; `.gitignore`
//!   decides for them.
//! - Every pruned directory, hidden file and symlink is recorded with a
//!   reason (pruned directories with file and byte counts).
//! - Symlinks to files inside the root are followed once (deduplicated
//!   against their target); links that leave the root are recorded.
//! - Entries hidden by ignore rules are counted per rule, and ancestor
//!   ignore files apply only when the root is inside a git worktree.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrd};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use ignore::WalkBuilder;
use phf::phf_set;
use rayon::prelude::*;
use serde::Serialize;

use crate::security::{has_reparse_point_in_path, sensitive_reason};

#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
/// OneDrive/cloud placeholder file: reading it would force a download.
#[cfg(windows)]
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
/// Cloud folder whose listing is not on this device: opening it downloads it.
#[cfg(windows)]
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
/// macOS: an iCloud Drive file or folder whose data is not on this Mac.
#[cfg(target_os = "macos")]
const SF_DATALESS: u32 = 0x4000_0000;

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
    // Code — C/C++ variants, GPU, .NET/Qt UI (N-05: were sniffed, and dropped above 500 KB)
    "cc", "cxx", "hh", "hxx", "inl", "ipp", "cu", "cuh", "glsl", "hlsl", "wgsl", "metal",
    "csproj", "fsproj", "vbproj", "sln", "props", "targets", "xaml", "razor", "cshtml",
    "qml", "ui", "svg",
    // Logs, subtitles, mail
    "log", "srt", "vtt", "eml",
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
    // MetaTrader: MQL sources, presets, charts, templates (often UTF-16; a big
    // EA was dropped as "large file with unknown extension" under a size cap)
    "mq4", "mq5", "mqh", "set", "chr", "tpl",
    // Text any editor opens: notebooks, calendars, contacts, subtitles, LaTeX
    // styles, XML dialects, geodata, R/Quarto/MDX documents
    "ipynb", "rtf", "ics", "vcf", "desktop", "nfo", "ass", "ssa",
    "sty", "cls", "dtd", "xsd", "xsl", "xslt", "wsdl", "rss", "atom",
    "kml", "gpx", "geojson", "mdx", "rmd", "qmd",
};

/// Known binary file extensions — always skip (no content sniffing needed)
static BINARY_EXTENSIONS: phf::Set<&'static str> = phf_set! {
    // Executables & libraries
    "exe", "dll", "so", "dylib", "bin", "com", "msi", "app",
    // Object files & bytecode
    "o", "obj", "lib", "a", "class", "pyc", "pyo", "wasm",
    // Images
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "webp",
    "tiff", "tif", "psd", "raw", "heic", "heif", "avif",
    // Audio
    "mp3", "wav", "flac", "aac", "ogg", "wma", "m4a", "opus",
    "aiff", "aif", "mid", "midi", "amr",
    // Video ("ts" is TypeScript and "sub" can be text subtitles: both stay
    // with the text list / the sniff)
    "mp4", "avi", "mkv", "mov", "wmv", "flv", "webm", "m4v", "mpeg", "mpg",
    "3gp", "3g2", "mts", "m2ts", "vob",
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

/// Version-control metadata: pruned everywhere, never counted.
const VCS_DIRS: &[&str] = &[".git", ".svn", ".hg", ".bzr"];

/// Credential directories: pruned everywhere, even with hidden files on.
const CREDENTIAL_DIRS: &[&str] = &[".ssh", ".aws", ".gnupg"];

/// Directory names that never hold a project's own source: dependency trees,
/// tool caches, build caches, OS junk, TurboMerger's own state. Pruned at any
/// depth. Ambiguous names (`build`, `dist`, `out`, `debug`, `release`,
/// `packages`, `vendor`, `coverage`, `env`, `obj`, `target`…) are NOT here —
/// `.gitignore` and the markers in `prune_dir_reason` decide for those.
static SKIP_DIRS_ALWAYS: phf::Set<&'static str> = phf_set! {
    // Dependencies
    "node_modules", ".pnpm-store", "bower_components",
    // Framework/tool caches
    ".parcel-cache", ".next", ".nuxt", ".svelte-kit", ".turbo", ".docusaurus",
    ".nyc_output", "htmlcov", "storybook-static",
    ".terraform", ".serverless", ".vercel", ".netlify",
    // OS junk
    "__macosx", "$recycle.bin", "system volume information",
    // TurboMerger's own apply-back backups (T3-3)
    ".turbomerger",
};

/// Python caches and dependency dirs — only skipped when include_venv=false.
/// Virtualenv ROOTS are detected by their `pyvenv.cfg` / `conda-meta` marker.
static SKIP_DIRS_VENV: phf::Set<&'static str> = phf_set! {
    "site-packages", "__pycache__", ".pytest_cache", ".mypy_cache", ".ruff_cache",
    ".tox", ".nox", ".eggs", "pip-wheel-metadata",
};

/// Venv names used only by the harvest-only credential walk (speed); the main
/// scan identifies virtualenvs by marker instead.
static VENV_NAMES: phf::Set<&'static str> = phf_set! {
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
    /// Absolute per-file cap in bytes (config `max_file_size_mb`); 0, the
    /// default, means no cap — big files are streamed, never dropped.
    pub max_file_size: u64,
    pub extra_text_exts: Vec<String>,
    pub extra_skip_exts: Vec<String>,
    pub extra_binary_exts: Vec<String>,
    /// Whitelist globs — if non-empty, ONLY matching files are kept.
    pub include_globs: Vec<String>,
    /// Blacklist globs — matching files are dropped.
    pub exclude_globs: Vec<String>,
    /// Read cloud files that are not on this device (downloads them). Off:
    /// they are recorded as not captured (N-10, N-11).
    pub hydrate: bool,
    /// Leave credential files out by name (`.env`, key material, credential
    /// stores). Follows secret redaction: off, every text file is taken.
    pub protect_secrets: bool,
    /// Whole-drive scan: skip pseudo file systems (`/proc`, `/sys`, `/dev`,
    /// snap images, …) and the OS's swap, hibernation and recycle areas.
    pub whole_drive: bool,
    /// Text-from-images is on: images go to the merger for OCR instead of
    /// being recorded as photos/binary.
    pub ocr_images: bool,
    /// With OCR on, read thumbnails, icons, figure crops of a Markdown
    /// export and MonadDoc work folders too (they are left out by default:
    /// `extract::non_content_picture`).
    pub ocr_all_pictures: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            include_venv: false,
            content_sniff: true,
            include_hidden: false,
            respect_gitignore: true,
            max_file_size: 0,
            extra_text_exts: Vec::new(),
            extra_skip_exts: Vec::new(),
            extra_binary_exts: Vec::new(),
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
            hydrate: false,
            protect_secrets: true,
            whole_drive: false,
            ocr_images: false,
            ocr_all_pictures: false,
        }
    }
}

/// Statistics about how files were detected during scanning
#[derive(Debug, Clone, Default, Serialize)]
pub struct ScanStats {
    pub by_extension: usize,
    pub by_content: usize,
    /// Documents (PDF, Office, e-book) whose text is extracted at merge time.
    pub by_document: usize,
    pub skipped_binary: usize,
    pub unreadable: usize,
    /// Directories not descended into (dependency trees, caches, hidden…).
    pub pruned_dirs: usize,
    /// Files and directories hidden by ignore rules / user globs.
    pub ignored_entries: usize,
    /// Symlinks met during the walk (followed or recorded).
    pub symlinks: usize,
}

/// Why something is not in the merge — decides the exit code (N-31).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipKind {
    /// Left out on purpose: sensitive files, lockfiles, previous outputs,
    /// hidden files, minified bundles, user globs, VCS/dependency dirs.
    Excluded,
    /// Binary by design (images, archives, executables, fonts, databases).
    Binary,
    /// Content that should have been captured but was not: unsupported
    /// documents and photos, too large, unreadable, credential-dense.
    NotCaptured,
    /// A directory that was not descended into (counts in the reason).
    PrunedDir,
    /// Entries hidden by one ignore rule (counts in the reason).
    IgnoredByRule,
}

/// A skipped file (or pruned directory, or ignore-rule summary) plus the
/// reason — feeds the output's Merge Report so nothing is ever dropped
/// invisibly.
#[derive(Debug, Clone, Serialize)]
pub struct SkipEntry {
    pub path: String,
    pub reason: String,
    pub kind: SkipKind,
}

impl SkipEntry {
    pub fn new(path: impl Into<String>, reason: impl Into<String>, kind: SkipKind) -> SkipEntry {
        SkipEntry {
            path: path.into(),
            reason: reason.into(),
            kind,
        }
    }
}

/// Complete scan result
pub struct ScanResult {
    pub files: Vec<PathBuf>,
    pub stats: ScanStats,
    pub skipped: Vec<SkipEntry>,
}

/// Explicit mode (ADR 0020): classify exactly the named files, never walk.
/// Paths are relative to `root` ('/' separators; an absolute path must lie
/// inside the root). `..`, paths resolving outside the root (symlinks
/// included) and missing files are recorded as not captured; a path listed
/// twice is merged once.
pub fn scan_explicit(
    root: &Path,
    paths: &[String],
    options: &ScanOptions,
) -> std::io::Result<ScanResult> {
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    let mut stats = ScanStats::default();
    let root_canon = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut seen = std::collections::HashSet::new();

    for rel_str in paths {
        let rel = if Path::new(rel_str).is_absolute() {
            match dunce::canonicalize(rel_str) {
                Ok(abs) => match abs.strip_prefix(&root_canon) {
                    Ok(r) => r.to_string_lossy().replace('\\', "/"),
                    Err(_) => {
                        skipped.push(SkipEntry::new(
                            rel_str.clone(),
                            "outside the source folder".to_string(),
                            SkipKind::NotCaptured,
                        ));
                        continue;
                    }
                },
                Err(_) => {
                    skipped.push(SkipEntry::new(
                        rel_str.clone(),
                        "file not found".to_string(),
                        SkipKind::NotCaptured,
                    ));
                    continue;
                }
            }
        } else {
            match crate::selection::normalize_rel(rel_str) {
                Ok(r) => r,
                Err(e) => {
                    skipped.push(SkipEntry::new(rel_str.clone(), e, SkipKind::NotCaptured));
                    continue;
                }
            }
        };
        if !seen.insert(rel.clone()) {
            continue;
        }
        let candidate = root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        if let Ok(real) = dunce::canonicalize(&candidate) {
            if !real.starts_with(&root_canon) {
                skipped.push(SkipEntry::new(
                    rel.clone(),
                    "resolves outside the source folder".to_string(),
                    SkipKind::NotCaptured,
                ));
                continue;
            }
        }
        let rel_str = &rel;

        let meta = match std::fs::metadata(&candidate) {
            Ok(m) => m,
            Err(_) => {
                skipped.push(SkipEntry::new(
                    rel_str.clone(),
                    "file not found".to_string(),
                    SkipKind::NotCaptured,
                ));
                continue;
            }
        };

        if !meta.is_file() {
            skipped.push(SkipEntry::new(
                rel_str.clone(),
                "not a regular file".to_string(),
                SkipKind::NotCaptured,
            ));
            continue;
        }

        let len = meta.len();

        match classify(&candidate, len, options) {
            Verdict::TextByExt => {
                files.push(candidate);
                stats.by_extension += 1;
            }
            Verdict::TextByContent => {
                files.push(candidate);
                stats.by_content += 1;
            }
            Verdict::Document => {
                files.push(candidate);
                stats.by_document += 1;
            }
            Verdict::Skip(reason, kind) => {
                if kind == SkipKind::Binary {
                    stats.skipped_binary += 1;
                }
                skipped.push(SkipEntry::new(rel_str.clone(), reason, kind));
            }
            Verdict::Unreadable => {
                stats.unreadable += 1;
                skipped.push(SkipEntry::new(
                    rel_str.clone(),
                    "unreadable",
                    SkipKind::NotCaptured,
                ));
            }
        }
    }

    Ok(ScanResult {
        files,
        stats,
        skipped,
    })
}

/// What a running scan reports (see `scan_with`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanProgress {
    /// Entries seen by the walk so far (the total is unknown until it ends).
    Walked(usize),
    /// Candidate files classified so far, of `total`.
    Classified { done: usize, total: usize },
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
    if VENV_NAMES.contains(lower_name) {
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

/// For other modules: does `name` look like one of our outputs?
pub fn is_own_output_name(name: &str) -> bool {
    is_own_output(&name.to_lowercase())
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
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let head = read_window(&mut file, 0)?;
    if head.is_empty() {
        return Ok(true); // Empty file is text
    }
    if !window_is_text(&head) {
        return Ok(false);
    }
    // No size cap stands behind this sniff any more, so a big file is judged
    // on its middle and its end too: a text header on a binary body (disk
    // images, database dumps) must not be read in full.
    if len > 4 * SNIFF_SIZE as u64 {
        let utf16 = crate::security::utf16_text_encoding(&head).map(|(enc, _)| enc);
        for at in [len / 2, len - SNIFF_SIZE as u64] {
            let mut w = read_window(&mut file, at & !1)?;
            let text = match utf16 {
                Some(enc) => crate::security::utf16_window_is_text(enc, &w),
                None => {
                    // Start on a character boundary: skip UTF-8 continuation bytes.
                    let skip = w.iter().take(3).take_while(|&&b| b & 0xC0 == 0x80).count();
                    w.drain(..skip);
                    window_is_text(&w)
                }
            };
            if !text {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Up to `SNIFF_SIZE` bytes starting at `at`.
fn read_window(file: &mut std::fs::File, at: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(at))?;
    let mut buffer = Vec::with_capacity(SNIFF_SIZE);
    file.by_ref()
        .take(SNIFF_SIZE as u64)
        .read_to_end(&mut buffer)?;
    Ok(buffer)
}

/// One sniff window: binary signatures and NUL density first, then UTF-8 or
/// UTF-16 validity, then byte statistics for legacy 8-bit encodings.
fn window_is_text(buffer: &[u8]) -> bool {
    let bytes_read = buffer.len();
    if bytes_read == 0 {
        return true;
    }
    if crate::security::is_binary_content(buffer) {
        return false;
    }
    // Well-formed UTF-8 is text: CJK prose is mostly high bytes, notebooks
    // and generated JSON have very long lines (N-04). UTF-16 likewise. The
    // statistics below are for legacy 8-bit encodings only.
    if crate::security::is_valid_text_window(buffer)
        || crate::security::utf16_text_encoding(buffer).is_some()
    {
        return true;
    }

    let control_count = buffer
        .iter()
        .filter(|&&b| (0x01..=0x08).contains(&b) || (0x0E..=0x1F).contains(&b))
        .count();
    if control_count * 100 > bytes_read * CONTROL_CHAR_THRESHOLD_PCT {
        return false;
    }

    let high_byte_count = buffer.iter().filter(|&&b| b >= 0x80).count();
    if high_byte_count * 100 > bytes_read * NON_ASCII_THRESHOLD_PCT {
        return false;
    }

    let max_line_len = buffer
        .split(|&b| b == b'\n')
        .map(|line| line.len())
        .max()
        .unwrap_or(0);
    max_line_len <= MAX_LINE_LENGTH
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

/// Why a directory is not descended into, or `None` to walk it. Cheap: name
/// checks plus at most a few `stat`s for markers.
fn prune_dir_reason(
    path: &Path,
    name_lower: &str,
    include_venv: bool,
    include_hidden: bool,
) -> Option<&'static str> {
    if VCS_DIRS.contains(&name_lower) {
        return Some("version-control metadata");
    }
    if CREDENTIAL_DIRS.contains(&name_lower) {
        return Some("credential directory — never scanned");
    }
    if SKIP_DIRS_ALWAYS.contains(name_lower) {
        return Some("dependency / tool-cache directory");
    }
    if name_lower == "target"
        && path
            .parent()
            .is_some_and(|p| p.join("Cargo.toml").is_file())
    {
        return Some("Rust build output (next to Cargo.toml)");
    }
    if path.join("CACHEDIR.TAG").is_file() {
        return Some("cache directory (CACHEDIR.TAG)");
    }
    if !include_venv {
        if SKIP_DIRS_VENV.contains(name_lower) {
            return Some("Python cache / dependency directory (--include-venv to scan)");
        }
        if path.join("pyvenv.cfg").is_file() {
            return Some("Python virtual environment (pyvenv.cfg; --include-venv to scan)");
        }
        if path.join("conda-meta").is_dir() {
            return Some("conda environment (conda-meta; --include-venv to scan)");
        }
    }
    if name_lower.starts_with('.') && !include_hidden && !DOT_DIR_ALLOWLIST.contains(&name_lower) {
        return Some("hidden directory (--include-hidden to scan)");
    }
    None
}

/// Something the walk filter left out, recorded for the report.
/// A pipe, socket or device node, by name; `None` for files, folders, links.
fn special_file_kind(t: std::fs::FileType) -> Option<&'static str> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if t.is_fifo() {
            return Some("named pipe (FIFO)");
        }
        if t.is_socket() {
            return Some("socket");
        }
        if t.is_char_device() {
            return Some("character device");
        }
        if t.is_block_device() {
            return Some("block device");
        }
    }
    let _ = t;
    None
}

/// What a whole-drive scan leaves out, by path: file systems that hold no
/// files (`/proc`, `/sys`, `/dev`, tmpfs, cgroup, snap and docker images),
/// network shares, and the OS's paging, hibernation, restore and recycle
/// areas. Linux reads its mount table, so such a file system is skipped
/// wherever it is mounted; macOS and Windows use fixed places.
fn system_areas(root: &Path) -> HashMap<PathBuf, &'static str> {
    let mut out: HashMap<PathBuf, &'static str> = HashMap::new();
    let mut add = |p: PathBuf, why: &'static str| {
        if p != root && p.starts_with(root) {
            out.entry(p).or_insert(why);
        }
    };
    #[cfg(target_os = "linux")]
    if let Ok(table) = std::fs::read_to_string("/proc/self/mountinfo") {
        for line in table.lines() {
            // `36 35 98:0 /mnt1 /mnt/parent rw,noatime master:1 - ext3 /dev/root rw`
            let Some((left, right)) = line.split_once(" - ") else {
                continue;
            };
            let (Some(mount_point), Some(fstype)) =
                (left.split(' ').nth(4), right.split(' ').next())
            else {
                continue;
            };
            if let Some(why) = linux_fs_reason(fstype) {
                add(PathBuf::from(unescape_mount_path(mount_point)), why);
            }
        }
    }
    #[cfg(target_os = "macos")]
    for (p, why) in [
        (
            "/System/Volumes",
            "system volumes (the data volume repeats /Users …) — not scanned",
        ),
        ("/Volumes", "other mounted volumes — not scanned"),
        ("/dev", "device files — not scanned"),
        (
            "/private/var/vm",
            "virtual-memory swap and sleep image — not scanned",
        ),
        ("/.vol", "volume-ID file system — not scanned"),
        ("/.fseventsd", "file-system event log — not scanned"),
        ("/.Spotlight-V100", "Spotlight index — not scanned"),
        (
            "/.DocumentRevisions-V100",
            "document-version store — not scanned",
        ),
        ("/cores", "crash core dumps — not scanned"),
    ] {
        add(PathBuf::from(p), why);
    }
    #[cfg(windows)]
    for name in [
        "$Recycle.Bin",
        "System Volume Information",
        "pagefile.sys",
        "hiberfil.sys",
        "swapfile.sys",
        "DumpStack.log.tmp",
        "Config.Msi",
        "$WinREAgent",
        "$Windows.~BT",
        "$Windows.~WS",
    ] {
        add(
            root.join(name),
            "Windows paging, hibernation, restore or recycle area — not scanned",
        );
    }
    out
}

/// Why a Linux file-system type is not scanned on a whole-drive run.
#[cfg(any(target_os = "linux", test))]
fn linux_fs_reason(fstype: &str) -> Option<&'static str> {
    match fstype {
        "proc" | "sysfs" | "devtmpfs" | "devpts" | "tmpfs" | "ramfs" | "cgroup" | "cgroup2"
        | "securityfs" | "pstore" | "efivarfs" | "bpf" | "debugfs" | "tracefs" | "configfs"
        | "fusectl" | "mqueue" | "hugetlbfs" | "autofs" | "binfmt_misc" | "nsfs" | "rpc_pipefs"
        | "selinuxfs" => {
            Some("virtual file system (kernel, devices, memory) — holds no files, not scanned")
        }
        "squashfs" | "overlay" => {
            Some("read-only image (snap package, container layer) — not scanned")
        }
        "nfs" | "nfs4" | "cifs" | "smb3" | "smbfs" | "fuse.sshfs" | "fuse.rclone" | "davfs"
        | "fuse.gvfsd-fuse" => Some("network share — not scanned"),
        _ => None,
    }
}

/// `/proc/self/mountinfo` writes space, tab, newline and backslash as
/// three-digit octal escapes.
#[cfg(any(target_os = "linux", test))]
fn unescape_mount_path(p: &str) -> String {
    p.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

struct Pruned {
    path: PathBuf,
    reason: &'static str,
    is_dir: bool,
    symlink: bool,
    /// Counting its contents would download it (cloud folders).
    no_count: bool,
}

/// Walk filter: `None` = keep walking, `Some(p)` = prune and record.
fn prune_entry(
    entry: &ignore::DirEntry,
    include_venv: bool,
    include_hidden: bool,
    hydrate: bool,
    protect_secrets: bool,
) -> Option<Pruned> {
    let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
    let pruned = |reason: &'static str, symlink: bool| Pruned {
        path: entry.path().to_path_buf(),
        reason,
        is_dir,
        symlink,
        no_count: false,
    };
    // Symlinks and, on Windows, junctions: std reports name-surrogate
    // reparse points as links. Other reparse points (OneDrive placeholders,
    // dedup) are ordinary files and folders (N-10).
    if entry.path_is_symlink() {
        return Some(pruned("symlink", true));
    }
    let name_lower = entry.file_name().to_string_lossy().to_lowercase();

    if is_dir {
        if !hydrate && is_cloud_only_dir(entry) {
            return Some(Pruned {
                no_count: true,
                ..pruned(
                    "cloud folder not on this device — not scanned (--hydrate downloads it)",
                    false,
                )
            });
        }
        return prune_dir_reason(entry.path(), &name_lower, include_venv, include_hidden)
            .map(|r| pruned(r, false));
    }

    // Files: hidden gate (dot-prefix) with the config-file allowlist. When
    // secrets are protected, sensitive files (.env, *.pem, id_rsa…) are let
    // through even when hidden so they get RECORDED as skipped-with-reason in
    // classify(); otherwise they are hidden files like any other.
    if name_lower.starts_with('.')
        && !include_hidden
        && !is_allowlisted_dotfile(&name_lower)
        && (!protect_secrets || sensitive_reason(entry.path()).is_none())
    {
        return Some(pruned("hidden file (--include-hidden to include)", false));
    }
    None
}

/// A folder whose listing lives only in the cloud (Windows RECALL_ON_OPEN,
/// macOS dataless): listing it would download it.
#[allow(unused_variables)]
fn is_cloud_only_dir(entry: &ignore::DirEntry) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if let Ok(meta) = entry.metadata() {
            return meta.file_attributes()
                & (FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
                != 0;
        }
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt;
        if let Ok(meta) = entry.metadata() {
            return meta.st_flags() & SF_DATALESS != 0;
        }
    }
    false
}

enum Verdict {
    TextByExt,
    TextByContent,
    /// A document (PDF, Office, e-book) whose text is extracted at merge time
    /// (Phase 3, ADR 0005) instead of being read as plain bytes. The merger
    /// re-derives the exact format from the extension, so no payload is kept.
    Document,
    Skip(String, SkipKind),
    Unreadable,
}

/// Content-bearing formats whose text is not read: photos without OCR
/// (`--extract-images-text` reads them) and archive formats TurboMerger
/// cannot open. They are reported as NOT captured — never as a silent
/// "binary" skip — so a folder of them no longer exits 0 (N-06, N-53).
/// Documents (PDF, Office old and new, OpenDocument, EPUB) are handled by
/// `crate::extract`; gzip, zip and tar by the merger's streaming path.
fn not_yet_extractable(ext_lower: &str) -> Option<&'static str> {
    match ext_lower {
        "heic" | "heif" | "jpg" | "jpeg" | "tif" | "tiff" => {
            Some("photo — its text needs OCR (--extract-images-text)")
        }
        "7z" | "rar" | "xz" | "bz2" | "zst" | "lz4" | "lzma" | "cab" => {
            Some("compressed archive in a format TurboMerger cannot open — extract it first")
        }
        _ => None,
    }
}

/// gzip, zip and tar (plain or gzip): opened at merge time, their text is
/// merged and binary members are counted (`merger::stream`).
fn is_openable_archive(name_lower: &str) -> bool {
    [".gz", ".tgz", ".tar", ".zip"]
        .iter()
        .any(|s| name_lower.ends_with(s))
}

/// Decide whether a single candidate file is merged. Runs on rayon threads.
///
/// Order matters for the exit code: what a file *is* comes before how big it
/// is. A 100 MB `.zip` or a large file of zeros is binary — excluded on
/// purpose — not text that was "too large" to capture (which exits 3).
fn classify(path: &Path, len: u64, options: &ScanOptions) -> Verdict {
    let name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => return Verdict::Skip("unrepresentable file name".into(), SkipKind::NotCaptured),
    };
    let name_lower = name.to_lowercase();

    if SKIP_FILES.iter().any(|&s| s == name_lower) {
        return Verdict::Skip(
            "lock/system file (context bloat)".into(),
            SkipKind::Excluded,
        );
    }
    if is_own_output(&name_lower) {
        return Verdict::Skip("previous TurboMerger output".into(), SkipKind::Excluded);
    }
    if is_minified_filename(name) {
        return Verdict::Skip("minified/bundled".into(), SkipKind::Excluded);
    }
    if options.protect_secrets {
        if let Some(reason) = sensitive_reason(path) {
            return Verdict::Skip(reason.to_string(), SkipKind::Excluded);
        }
    }
    let ext_lower = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    if let Some(ext) = ext_lower.as_deref() {
        // Documents are extractable now (Phase 3, ADR 0005): route them to the
        // merger instead of dropping them. The per-file size cap still applies
        // (0 = unlimited) so a 2 GB PDF does not blow the run.
        if crate::extract::DocKind::from_ext(ext).is_some() {
            if options.max_file_size != 0 && len > options.max_file_size {
                return Verdict::Skip(
                    format!("too large ({} KB)", len / 1024),
                    SkipKind::NotCaptured,
                );
            }
            return Verdict::Document;
        }
        let picture = options.ocr_images && crate::extract::is_ocr_image(ext);
        if is_openable_archive(&name_lower) || picture {
            if options.max_file_size != 0 && len > options.max_file_size {
                return Verdict::Skip(
                    format!("too large ({} KB)", len / 1024),
                    SkipKind::NotCaptured,
                );
            }
            if picture && !options.ocr_all_pictures {
                if let Some(why) = crate::extract::non_content_picture(path) {
                    return Verdict::Skip(why, SkipKind::Excluded);
                }
            }
            return Verdict::Document;
        }
        if let Some(reason) = not_yet_extractable(ext) {
            return Verdict::Skip(reason.into(), SkipKind::NotCaptured);
        }
    }

    // Known by name or extension: user overrides, binary types, text types.
    let known_text = match ext_lower.as_deref() {
        Some(ext) => {
            // Step 1: Config exclude list (highest priority user override)
            if options.extra_skip_exts.iter().any(|e| e == ext) {
                return Verdict::Skip("excluded by turbomerger.toml".into(), SkipKind::Excluded);
            }
            // Step 2: Known binary extension → skip, whatever its size
            if BINARY_EXTENSIONS.contains(ext) || options.extra_binary_exts.iter().any(|e| e == ext)
            {
                return Verdict::Skip("binary extension".into(), SkipKind::Binary);
            }
            TEXT_EXTENSIONS.contains(ext) || options.extra_text_exts.iter().any(|e| e == ext)
        }
        None => is_known_extensionless_file(&name_lower),
    };

    // Unknown type: the first 8 KB decide text vs binary, whatever its size.
    let mut by_content = false;
    if !known_text {
        if !options.content_sniff {
            let why = if ext_lower.is_some() {
                "unknown extension (content detection off)"
            } else {
                "no extension (content detection off)"
            };
            return Verdict::Skip(why.into(), SkipKind::NotCaptured);
        }
        match sniff_file_content(path) {
            Ok(true) => by_content = true,
            Ok(false) => return Verdict::Skip("binary content".into(), SkipKind::Binary),
            Err(_) => return Verdict::Unreadable,
        }
    }

    // Text that is too big to capture (0 = unlimited).
    if options.max_file_size != 0 && len > options.max_file_size {
        return Verdict::Skip(
            format!("too large ({} KB)", len / 1024),
            SkipKind::NotCaptured,
        );
    }
    if options.max_file_size != 0 && by_content && len > LARGE_FILE_UNKNOWN_EXT {
        return Verdict::Skip(
            "large file with unknown extension".into(),
            SkipKind::NotCaptured,
        );
    }
    if by_content {
        Verdict::TextByContent
    } else {
        Verdict::TextByExt
    }
}

// ============================================================================
// CREDENTIAL FILE DISCOVERY (harvest-only)
// ============================================================================

/// Find credential-store DOCUMENT files by name, **ignoring .gitignore** —
/// credential files (`MASTER_CREDENTIALS_*.md`, `.env`, `credentials.json`)
/// are almost always gitignored, so the normal scan never sees them. The
/// merger reads these *harvest-only* (to redact their values from prose
/// echoes elsewhere) and NEVER merges their content. Key/cert/SSH material is
/// excluded — a private-key body doesn't echo as prose and would add noise.
/// Bounded to a modest size; always-skip dirs are still pruned for speed.
pub fn find_credential_files(root: &Path) -> Vec<PathBuf> {
    const HARVEST_MAX_BYTES: u64 = 4 * 1024 * 1024;
    let mut builder = WalkBuilder::new(root);
    builder
        .follow_links(false)
        .hidden(false)
        .require_git(false)
        .git_ignore(false) // the whole point: see gitignored credential files
        .git_exclude(false)
        .ignore(false)
        .parents(false)
        .git_global(false);
    builder.filter_entry(|entry| {
        if entry.depth() == 0 {
            return true;
        }
        if entry.path_is_symlink() {
            return false;
        }
        let name_lower = entry.file_name().to_string_lossy().to_lowercase();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            // Never download a cloud-only folder just to look for secrets.
            if is_cloud_only_dir(entry) {
                return false;
            }
            // Prune noise dirs and venvs; keep everything else, dotdirs included.
            // Build outputs never hold credential files; skipping them keeps
            // this gitignore-bypassing walk fast.
            return !VCS_DIRS.contains(&name_lower.as_str())
                && !SKIP_DIRS_ALWAYS.contains(name_lower.as_str())
                && !matches!(
                    name_lower.as_str(),
                    "target" | "dist" | "build" | "out" | "_build" | "obj" | "coverage"
                )
                && !is_venv_by_name(&name_lower);
        }
        true
    });

    let mut out = Vec::new();
    for result in builder.build() {
        let Ok(entry) = result else { continue };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        // Only credential DOCUMENTS whose values plausibly echo as text.
        let harvestable = match crate::security::sensitive_reason(path) {
            Some(r) => {
                r.starts_with("env file")
                    || r == "credential store"
                    || r == "credential/secret data file"
            }
            None => false,
        };
        if !harvestable {
            continue;
        }
        if entry
            .metadata()
            .map(|m| m.is_file() && m.len() <= HARVEST_MAX_BYTES)
            .unwrap_or(false)
        {
            out.push(path.to_path_buf());
        }
    }
    out
}

// ============================================================================
// MAIN SCANNER
// ============================================================================

/// True when `root` is inside a git worktree (a `.git` dir or file at the
/// root or any ancestor). Ancestor ignore files only apply there (N-03): a
/// dotfiles repo in `$HOME` must not hide files in `~/Desktop/papers`.
pub fn inside_git_worktree(root: &Path) -> bool {
    root.ancestors().any(|a| a.join(".git").exists())
}

/// Per-directory counting budget for pruned/ignored directories, and a global
/// cap so fifty `node_modules` cannot turn the report into the slow part.
const COUNT_CAP_PER_DIR: usize = 10_000;
const COUNT_CAP_TOTAL: usize = 100_000;

/// Files and bytes under `dir`, without following links. `None` when the
/// global budget is spent; `capped` when this directory hit its own cap.
fn count_tree(dir: &Path, budget: &mut usize, cancel: &AtomicBool) -> Option<(usize, u64, bool)> {
    if *budget == 0 {
        return None;
    }
    let (mut files, mut bytes, mut visited) = (0usize, 0u64, 0usize);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if cancel.load(AtomicOrd::Relaxed) {
            return None;
        }
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            visited += 1;
            *budget = budget.saturating_sub(1);
            if visited > COUNT_CAP_PER_DIR || *budget == 0 {
                return Some((files, bytes, true));
            }
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push(e.path());
            } else if ft.is_file() {
                files += 1;
                bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    Some((files, bytes, false))
}

/// Files and bytes under `dir` after at most `max_entries` directory entries,
/// without reading any file or following links: `(files, bytes, truncated)`.
/// The GUI calls it before deciding whether a folder may be scanned
/// automatically.
pub fn quick_count(dir: &Path, max_entries: usize) -> (usize, u64, bool) {
    let mut budget = max_entries;
    let cancel = AtomicBool::new(false);
    match count_tree(dir, &mut budget, &cancel) {
        Some((files, bytes, capped)) => (files, bytes, capped || budget == 0),
        None => (0, 0, true),
    }
}

fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{} B", b)
    } else {
        format!("{:.1} {}", v, UNITS[u])
    }
}

fn count_note(counted: Option<(usize, u64, bool)>) -> String {
    match counted {
        None => "not counted".into(),
        Some((files, bytes, capped)) => format!(
            "{}{} file{}, {}{}",
            files,
            if capped { "+" } else { "" },
            if files == 1 && !capped { "" } else { "s" },
            human_bytes(bytes),
            if capped { "+" } else { "" }
        ),
    }
}

/// Finds which ignore rule hides an entry, for the report ("12 files ignored
/// by `profiles/` in .gitignore"). Approximate by design — deepest matching
/// ignore file wins — which is what the user needs to find the rule.
struct IgnoreAttribution {
    root: PathBuf,
    stop: Option<PathBuf>,
    cache: BTreeMap<PathBuf, Option<ignore::gitignore::Gitignore>>,
}

impl IgnoreAttribution {
    fn new(root: &Path, include_ancestors: bool) -> Self {
        // With ancestor ignore files on, look up to the worktree root.
        let stop = if include_ancestors {
            root.ancestors()
                .find(|a| a.join(".git").exists())
                .map(Path::to_path_buf)
        } else {
            None
        };
        IgnoreAttribution {
            root: root.to_path_buf(),
            stop,
            cache: BTreeMap::new(),
        }
    }

    fn matcher(&mut self, file: PathBuf) -> Option<&ignore::gitignore::Gitignore> {
        self.cache
            .entry(file.clone())
            .or_insert_with(|| {
                if !file.is_file() {
                    return None;
                }
                let dir = if file.ends_with("info/exclude") {
                    file.parent()?.parent()?.parent()?.to_path_buf()
                } else {
                    file.parent()?.to_path_buf()
                };
                let mut b = ignore::gitignore::GitignoreBuilder::new(dir);
                b.add(&file);
                b.build().ok()
            })
            .as_ref()
    }

    /// `(source file relative to the root, pattern)` of the rule that hides `path`.
    fn rule_for(&mut self, path: &Path, is_dir: bool) -> Option<(String, String)> {
        let top = self.stop.clone().unwrap_or_else(|| self.root.clone());
        let mut dirs: Vec<PathBuf> = Vec::new();
        for a in path.ancestors().skip(1) {
            dirs.push(a.to_path_buf());
            if a == top {
                break;
            }
        }
        for dir in dirs {
            let mut files = vec![
                dir.join(".turbomergerignore"),
                dir.join(".ignore"),
                dir.join(".gitignore"),
            ];
            if dir == top || dir.join(".git").is_dir() {
                files.push(dir.join(".git").join("info").join("exclude"));
            }
            for file in files {
                let Ok(rel) = path.strip_prefix(&dir) else {
                    continue;
                };
                let rel = rel.to_path_buf();
                let pattern = {
                    let Some(gi) = self.matcher(file.clone()) else {
                        continue;
                    };
                    match gi.matched_path_or_any_parents(&rel, is_dir) {
                        ignore::Match::Ignore(glob) => glob.original().to_string(),
                        _ => continue,
                    }
                };
                {
                    let source = file
                        .strip_prefix(&self.root)
                        .map(|p| p.to_string_lossy().replace('\\', "/"))
                        .unwrap_or_else(|_| {
                            format!(
                                "{} (outside the scanned folder)",
                                file.file_name().unwrap_or_default().to_string_lossy()
                            )
                        });
                    return Some((source, pattern));
                }
            }
        }
        None
    }
}

/// Scan directory for text files: gitignore-aware walk, then parallel
/// classification with per-file skip reasons. Nothing leaves the scan
/// unrecorded: pruned directories, hidden files, symlinks and ignored
/// entries all land in `skipped` (N-01/N-02/N-03).
pub fn scan_text_files(root: &Path, options: &ScanOptions) -> Result<ScanResult> {
    scan_with(root, options, &AtomicBool::new(false), &|_| {})
}

/// `scan_text_files` for a job: reports progress, and stops with
/// `Err(Cancelled)` soon after `cancel` is set (checked per entry).
pub fn scan_with(
    root: &Path,
    options: &ScanOptions,
    cancel: &AtomicBool,
    progress: &(dyn Fn(ScanProgress) + Sync),
) -> Result<ScanResult> {
    let stop = || -> Result<ScanResult> { Err(crate::cancel::Cancelled.into()) };
    if has_reparse_point_in_path(root).unwrap_or(true) {
        anyhow::bail!("Root path contains junction points or symlinks");
    }
    let root_canon = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let use_ancestors = options.respect_gitignore && inside_git_worktree(root);

    let mut builder = WalkBuilder::new(root);
    builder
        .follow_links(false)
        .hidden(false) // hidden handling is ours (dot allowlist in prune_entry)
        .require_git(false) // honor .gitignore even outside a git repo
        .git_ignore(options.respect_gitignore)
        .git_exclude(options.respect_gitignore)
        .ignore(options.respect_gitignore)
        .parents(use_ancestors)
        .git_global(false); // deterministic: user-global excludes don't apply
    builder.add_custom_ignore_filename(".turbomergerignore");

    // User include/exclude globs via ripgrep's override layer. A non-empty
    // whitelist means only matching files survive; `!glob` entries are excludes.
    let mut overrides: Option<ignore::overrides::Override> = None;
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
        let built = ob
            .build()
            .map_err(|e| anyhow::anyhow!("glob build failed: {}", e))?;
        builder.overrides(built.clone());
        overrides = Some(built);
    }

    let include_venv = options.include_venv;
    let include_hidden = options.include_hidden;
    let hydrate = options.hydrate;
    let protect_secrets = options.protect_secrets;
    let system_areas = if options.whole_drive {
        system_areas(&root_canon)
    } else {
        HashMap::new()
    };
    // Walk 2 must skip the same areas, or it reports /proc as "ignored".
    let walk2_areas = system_areas.clone();
    let pruned: Arc<Mutex<Vec<Pruned>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&pruned);
    builder.filter_entry(move |entry| {
        if entry.depth() == 0 {
            return true;
        }
        if let Some(reason) = system_areas.get(entry.path()) {
            if let Ok(mut v) = recorder.lock() {
                v.push(Pruned {
                    path: entry.path().to_path_buf(),
                    reason,
                    is_dir: entry.file_type().is_some_and(|t| t.is_dir()),
                    symlink: false,
                    // Never count inside /proc, /sys, a network share…
                    no_count: true,
                });
            }
            return false;
        }
        match prune_entry(
            entry,
            include_venv,
            include_hidden,
            hydrate,
            protect_secrets,
        ) {
            None => true,
            Some(p) => {
                if let Ok(mut v) = recorder.lock() {
                    v.push(p);
                }
                false
            }
        }
    });

    // Walk 1 (sequential, heavily pruned): collect candidate files.
    let mut candidates: Vec<(PathBuf, u64)> = Vec::new();
    let mut skipped: Vec<SkipEntry> = Vec::new();
    let mut stats = ScanStats::default();
    // Only walk 2 reads the set, and walk 2 runs only with ignore rules or
    // user globs: a default run (gitignored files included) keeps no copy
    // of the paths at all (S10, B11 ladder in the handoff §8).
    let walk2_possible = options.respect_gitignore || overrides.is_some();
    let mut seen = PathSet::default();
    let mut ignore_file_seen = false;

    let mut walked = 0usize;
    for result in builder.build() {
        if cancel.load(AtomicOrd::Relaxed) {
            return stop();
        }
        walked += 1;
        if walked.is_multiple_of(512) {
            progress(ScanProgress::Walked(walked));
        }
        let entry = match result {
            Ok(e) => e,
            Err(err) => {
                stats.unreadable += 1;
                skipped.push(SkipEntry::new(
                    scrub_root(&err.to_string(), root),
                    "unreadable during walk",
                    SkipKind::NotCaptured,
                ));
                continue;
            }
        };
        if walk2_possible {
            seen.insert(entry.path());
        }
        ignore_file_seen = ignore_file_seen || names_ignore_file(entry.path());
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            // Pipes, sockets and devices are never opened — reading a FIFO
            // blocks forever — but they are recorded, so every entry is
            // accounted for.
            if let Some(kind) = entry.file_type().and_then(special_file_kind) {
                skipped.push(SkipEntry::new(
                    relative_display(root, entry.path()),
                    format!("{kind} — not a regular file, not read"),
                    SkipKind::Excluded,
                ));
            }
            continue;
        }
        let path = entry.path().to_path_buf();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => {
                stats.unreadable += 1;
                skipped.push(SkipEntry::new(
                    relative_display(root, &path),
                    "metadata unreadable",
                    SkipKind::NotCaptured,
                ));
                continue;
            }
        };
        #[cfg(windows)]
        if !options.hydrate && meta.file_attributes() & FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS != 0 {
            skipped.push(SkipEntry::new(
                relative_display(root, &path),
                "cloud file not on this device — not read (--hydrate downloads it)",
                SkipKind::NotCaptured,
            ));
            continue;
        }
        #[cfg(target_os = "macos")]
        {
            use std::os::macos::fs::MetadataExt;
            if !options.hydrate && meta.st_flags() & SF_DATALESS != 0 {
                skipped.push(SkipEntry::new(
                    relative_display(root, &path),
                    "iCloud file not on this Mac — not read (--hydrate downloads it)",
                    SkipKind::NotCaptured,
                ));
                continue;
            }
        }
        candidates.push((path, meta.len()));
    }
    progress(ScanProgress::Walked(walked));

    // What the walk filter left out: directories (with counts), hidden files,
    // and symlinks (resolved below).
    let pruned = std::mem::take(&mut *pruned.lock().expect("prune recorder"));
    // Walk 2 attributes what ignore rules hid; with no ignore file anywhere
    // (and no ancestor rules), there is nothing for it to find (D4).
    let ignore_files_present =
        use_ancestors || ignore_file_seen || pruned.iter().any(|p| names_ignore_file(&p.path));
    let mut budget = COUNT_CAP_TOTAL;
    // Symlinks ask whether their target is merged under its own name; with
    // no symlink there is nothing to ask, so no second copy of every path.
    let mut candidate_rels = PathSet::default();
    if pruned.iter().any(|p| p.symlink) {
        for (p, _) in &candidates {
            candidate_rels.insert_str(&relative_display(root, p));
        }
    }
    for p in pruned {
        let rel = relative_display(root, &p.path);
        if p.symlink {
            stats.symlinks += 1;
            match resolve_symlink(&p.path, &root_canon) {
                LinkTarget::InRootFile(target_rel) if candidate_rels.contains_str(&target_rel) => {
                    skipped.push(SkipEntry::new(
                        rel,
                        format!("symlink to {} (content included there)", target_rel),
                        SkipKind::Excluded,
                    ));
                }
                LinkTarget::InRootFile(_) => {
                    // The target is not merged under its own name (hidden,
                    // ignored…): merge it once under the link's name.
                    let len = std::fs::metadata(&p.path).map(|m| m.len()).unwrap_or(0);
                    candidates.push((p.path, len));
                }
                LinkTarget::InRootDir(target_rel) => skipped.push(SkipEntry::new(
                    format!("{}/", rel),
                    format!("symlinked directory → {}/ — not followed", target_rel),
                    SkipKind::Excluded,
                )),
                LinkTarget::Outside => skipped.push(SkipEntry::new(
                    rel,
                    "symlink points outside the root — not followed",
                    SkipKind::Excluded,
                )),
                LinkTarget::Broken => {
                    skipped.push(SkipEntry::new(rel, "broken symlink", SkipKind::Excluded))
                }
            }
        } else if p.is_dir {
            stats.pruned_dirs += 1;
            let counted = if p.reason == "version-control metadata" || p.no_count {
                None
            } else {
                count_tree(&p.path, &mut budget, cancel)
            };
            let reason = if p.reason == "version-control metadata" || p.no_count {
                format!("{} — not scanned", p.reason)
            } else {
                format!("{} — not scanned ({})", p.reason, count_note(counted))
            };
            skipped.push(SkipEntry::new(
                format!("{}/", rel),
                reason,
                SkipKind::PrunedDir,
            ));
        } else {
            skipped.push(SkipEntry::new(rel, p.reason, SkipKind::Excluded));
        }
    }

    if cancel.load(AtomicOrd::Relaxed) {
        return stop();
    }

    // Walk 2: everything ignore rules or user globs hid, counted per rule.
    if (options.respect_gitignore && ignore_files_present) || overrides.is_some() {
        let ignored = find_ignored(
            root,
            seen,
            include_venv,
            include_hidden,
            protect_secrets,
            walk2_areas,
            cancel,
        );
        if cancel.load(AtomicOrd::Relaxed) {
            return stop();
        }
        stats.ignored_entries = ignored.len();
        let mut attribution = IgnoreAttribution::new(root, use_ancestors);
        // (source, pattern) -> (files, dirs, bytes, capped)
        let mut per_rule: BTreeMap<(String, String), (usize, usize, u64, bool)> = BTreeMap::new();
        for (path, is_dir) in ignored {
            let rel = relative_display(root, &path);
            let by_glob = overrides
                .as_ref()
                .map(|o| o.matched(&rel, is_dir))
                .is_some_and(|m| {
                    m.is_ignore() || (m.is_none() && !is_dir && o_has_whitelist(options))
                });
            let key = if by_glob {
                ("--include/--exclude".to_string(), "user glob".to_string())
            } else {
                attribution.rule_for(&path, is_dir).unwrap_or_else(|| {
                    (
                        "ignore rules".to_string(),
                        "(rule not identified)".to_string(),
                    )
                })
            };
            let slot = per_rule.entry(key).or_insert((0, 0, 0, false));
            if is_dir {
                slot.1 += 1;
                if let Some((f, b, capped)) = count_tree(&path, &mut budget, cancel) {
                    slot.0 += f;
                    slot.2 += b;
                    slot.3 |= capped;
                }
            } else {
                slot.0 += 1;
                slot.2 += std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            }
        }
        for ((source, pattern), (files, dirs, bytes, capped)) in per_rule {
            let mut what = format!(
                "{}{} file{}",
                files,
                if capped { "+" } else { "" },
                if files == 1 && !capped { "" } else { "s" }
            );
            if dirs > 0 {
                what.push_str(&format!(
                    " in {} director{}",
                    dirs,
                    if dirs == 1 { "y" } else { "ies" }
                ));
            }
            skipped.push(SkipEntry::new(
                format!("{}: {}", source, pattern),
                format!("ignored — {} ({})", what, human_bytes(bytes)),
                SkipKind::IgnoredByRule,
            ));
        }
    }

    if cancel.load(AtomicOrd::Relaxed) {
        return stop();
    }

    // Phase 2 (parallel): classify candidates (includes content sniffing).
    let total = candidates.len();
    let classified = AtomicUsize::new(0);
    let verdicts: Vec<(PathBuf, Verdict)> = candidates
        .into_par_iter()
        .map(|(path, len)| {
            if cancel.load(AtomicOrd::Relaxed) {
                return (path, Verdict::Unreadable);
            }
            let v = classify(&path, len, options);
            let done = classified.fetch_add(1, AtomicOrd::Relaxed) + 1;
            if done.is_multiple_of(256) || done == total {
                progress(ScanProgress::Classified { done, total });
            }
            (path, v)
        })
        .collect();
    if cancel.load(AtomicOrd::Relaxed) {
        return stop();
    }

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
            Verdict::Document => {
                stats.by_document += 1;
                files.push(path);
            }
            Verdict::Skip(reason, kind) => {
                if kind == SkipKind::Binary {
                    stats.skipped_binary += 1;
                }
                skipped.push(SkipEntry::new(relative_display(root, &path), reason, kind));
            }
            Verdict::Unreadable => {
                stats.unreadable += 1;
                skipped.push(SkipEntry::new(
                    relative_display(root, &path),
                    "unreadable",
                    SkipKind::NotCaptured,
                ));
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

/// An ignore file, or a `.git` directory (its `info/exclude`).
fn names_ignore_file(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|n| n.to_str()),
        Some(".gitignore" | ".ignore" | ".turbomergerignore" | ".git")
    )
}

/// A set of paths kept as their 128-bit XXH3, not as `PathBuf`s (HANDOFF_S8
/// 4.4, DeepSeek A2): 16 bytes a path instead of a 24-byte header plus a
/// heap copy of the path (≈ 100–200 B). Walk 1 fills it, walk 2 asks it.
/// No path is kept to confirm a hit: with N seen and M asked paths the
/// chance of any false hit is about N·M / 2¹²⁸ (≈ 10⁻²⁴ at 20 M each), and
/// a false hit would only move one ignored entry out of the ignore counts.
#[derive(Default)]
pub(crate) struct PathSet(HashSet<u128, std::hash::BuildHasherDefault<KeyIsHash>>);

impl PathSet {
    fn key(bytes: &[u8]) -> u128 {
        twox_hash::XxHash3_128::oneshot(bytes)
    }
    pub(crate) fn insert(&mut self, path: &Path) {
        self.0
            .insert(Self::key(path.as_os_str().as_encoded_bytes()));
    }
    pub(crate) fn contains(&self, path: &Path) -> bool {
        self.0
            .contains(&Self::key(path.as_os_str().as_encoded_bytes()))
    }
    fn insert_str(&mut self, s: &str) {
        self.0.insert(Self::key(s.as_bytes()));
    }
    fn contains_str(&self, s: &str) -> bool {
        self.0.contains(&Self::key(s.as_bytes()))
    }
    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.len()
    }
}

/// The set's keys are already XXH3 hashes: fold them instead of hashing
/// them again with SipHash.
#[derive(Default)]
pub(crate) struct KeyIsHash(u64);

impl std::hash::Hasher for KeyIsHash {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        // Only reached for keys other than u128 (none in this module).
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
        }
    }
    fn write_u128(&mut self, n: u128) {
        self.0 = (n as u64) ^ ((n >> 64) as u64);
    }
}

fn o_has_whitelist(options: &ScanOptions) -> bool {
    !options.include_globs.is_empty()
}

/// Walk errors embed absolute paths; the report shows root-relative ones.
fn scrub_root(msg: &str, root: &Path) -> String {
    let r = root.to_string_lossy();
    msg.replace(&format!("{}/", r), "")
        .replace(&format!("{}\\", r), "")
        .replace(r.as_ref(), ".")
}

enum LinkTarget {
    InRootFile(String),
    InRootDir(String),
    Outside,
    Broken,
}

fn resolve_symlink(link: &Path, root_canon: &Path) -> LinkTarget {
    let Ok(target) = dunce::canonicalize(link) else {
        return LinkTarget::Broken;
    };
    let Ok(rel) = target.strip_prefix(root_canon) else {
        return LinkTarget::Outside;
    };
    let rel = rel.to_string_lossy().replace('\\', "/");
    if target.is_dir() {
        LinkTarget::InRootDir(rel)
    } else if target.is_file() {
        LinkTarget::InRootFile(rel)
    } else {
        LinkTarget::Outside
    }
}

/// Entries that the ignore rules / user globs hid from walk 1: the same walk
/// without those rules, stopping at the first entry walk 1 never saw (an
/// ignored directory is reported once, not descended into).
fn find_ignored(
    root: &Path,
    // Taken, not borrowed: walk 1 is done with it, and a copy for the walk's
    // threads doubled its memory (S8, DeepSeek A2 — ~150–300 B per path).
    // Since S10 it holds 16-byte hashes, not paths.
    seen: PathSet,
    include_venv: bool,
    include_hidden: bool,
    protect_secrets: bool,
    system_areas: HashMap<PathBuf, &'static str>,
    cancel: &AtomicBool,
) -> Vec<(PathBuf, bool)> {
    let seen = Arc::new(seen);
    let found: Arc<Mutex<Vec<(PathBuf, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&found);
    let mut b = WalkBuilder::new(root);
    b.follow_links(false)
        .hidden(false)
        .require_git(false)
        .git_ignore(false)
        .git_exclude(false)
        .ignore(false)
        .parents(false)
        .git_global(false);
    b.filter_entry(move |entry| {
        if entry.depth() == 0 {
            return true;
        }
        // Pruned by our own rules: already recorded by walk 1 (or inside an
        // ignored directory, which is reported as a whole). Same rules as
        // walk 1: the whole-drive areas and the secrets switch included.
        if system_areas.contains_key(entry.path())
            || prune_entry(entry, include_venv, include_hidden, false, protect_secrets).is_some()
        {
            return false;
        }
        if seen.contains(entry.path()) {
            return true;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if let Ok(mut v) = sink.lock() {
            v.push((entry.path().to_path_buf(), is_dir));
        }
        false
    });
    for _ in b.build() {
        if cancel.load(AtomicOrd::Relaxed) {
            break;
        }
    }
    let mut out = std::mem::take(&mut *found.lock().expect("ignored sink"));
    out.sort();
    out
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_path_set_keeps_hashes_and_tells_paths_apart() {
        // S10 (4.4): walk 2's set holds 16-byte XXH3 keys, not paths.
        let mut set = PathSet::default();
        let paths = ["r/a", "r/a/b", "r/ab", "r/a b", "r/ä", "r/A"];
        for p in &paths[..3] {
            set.insert(Path::new(p));
        }
        assert_eq!(set.len(), 3);
        for p in &paths[..3] {
            assert!(set.contains(Path::new(p)), "{p}");
        }
        for p in &paths[3..] {
            assert!(!set.contains(Path::new(p)), "{p} is not in the set");
        }
        set.insert(Path::new("r/a"));
        assert_eq!(set.len(), 3, "a path inserted twice is one key");
        // Relative-path strings key the same bytes as the paths they name.
        let mut rels = PathSet::default();
        rels.insert_str("src/main.rs");
        assert!(rels.contains_str("src/main.rs") && rels.contains(Path::new("src/main.rs")));
        assert!(!rels.contains_str("src/main.r"));
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let latin1 = Path::new(std::ffi::OsStr::from_bytes(b"r/caf\xe9"));
            set.insert(latin1);
            assert!(set.contains(latin1));
            assert!(!set.contains(Path::new("r/café")), "bytes, not lossy text");
        }
        let mut h = KeyIsHash::default();
        std::hash::Hasher::write_u128(&mut h, 0x0123_4567_89ab_cdef_fedc_ba98_7654_3210);
        assert_eq!(
            std::hash::Hasher::finish(&h),
            0x0123_4567_89ab_cdef ^ 0xfedc_ba98_7654_3210
        );
    }

    #[test]
    fn walk_2_still_names_every_ignored_entry() {
        // The set changed from paths to hashes; what walk 2 reports did not.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("generated/deep")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join(".gitignore"), "generated/\n*.log\n").unwrap();
        std::fs::write(root.join("generated/deep/x.js"), "x\n").unwrap();
        std::fs::write(root.join("src/a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(root.join("src/b.log"), "log\n").unwrap();
        std::fs::write(root.join("src/c.log"), "log\n").unwrap();
        let opts = ScanOptions {
            respect_gitignore: true,
            ..Default::default()
        };
        let scan = scan_text_files(root, &opts).unwrap();
        let rels: Vec<String> = scan
            .files
            .iter()
            .map(|f| relative_display(root, f))
            .collect();
        assert!(rels.contains(&"src/a.rs".to_string()), "{rels:?}");
        assert!(!rels
            .iter()
            .any(|r| r.ends_with(".log") || r.starts_with("generated")));
        let rules: Vec<(&str, &str)> = scan
            .skipped
            .iter()
            .filter(|s| s.kind == SkipKind::IgnoredByRule)
            .map(|s| (s.path.as_str(), s.reason.as_str()))
            .collect();
        assert!(
            rules
                .iter()
                .any(|(p, r)| p.ends_with("generated/") && r.contains("1 file in 1 directory")),
            "{rules:?}"
        );
        assert!(
            rules
                .iter()
                .any(|(p, r)| p.ends_with("*.log") && r.contains("2 files")),
            "{rules:?}"
        );
    }

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
        assert!(VCS_DIRS.contains(&".git"));
        assert!(CREDENTIAL_DIRS.contains(&".ssh"));
        assert!(SKIP_DIRS_ALWAYS.contains(".turbomerger"));
        assert!(SKIP_DIRS_VENV.contains("__pycache__"));
        // N-01: ambiguous names are source in plenty of repos — never pruned
        // by name alone.
        for name in [
            "packages", "build", "dist", "out", "debug", "release", "vendor", "coverage", "env",
            "obj", "target", "venv", "lib64", "x64",
        ] {
            assert!(!SKIP_DIRS_ALWAYS.contains(name), "{name}");
            assert!(!SKIP_DIRS_VENV.contains(name), "{name}");
        }
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
        assert_eq!(opts.max_file_size, 0, "no size cap by default");
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
    fn test_sniff_is_validity_first() {
        // N-04: one long line (a notebook's embedded image, generated JSON) and
        // mostly-high-byte UTF-8 (CJK subtitles) are text, not binary.
        let dir = tempfile::tempdir().unwrap();
        let long = dir.path().join("bundle.xyz");
        std::fs::write(&long, "a".repeat(5000).as_bytes()).unwrap();
        assert!(sniff_file_content(&long).unwrap());
        let cjk = dir.path().join("lecture.sub");
        std::fs::write(&cjk, "这是一个关于机器学习的讲座字幕示例。\n".repeat(50)).unwrap();
        assert!(sniff_file_content(&cjk).unwrap());
        let magic = dir.path().join("MZ80_notes.txt2");
        std::fs::write(&magic, "MZ-80 emulator notes\nThis is plain text.\n").unwrap();
        assert!(sniff_file_content(&magic).unwrap());
        // Invalid UTF-8 full of control bytes is still binary.
        let bin = dir.path().join("blob.xyz");
        let data: Vec<u8> = (0..4096u32).map(|i| (i * 7 % 251) as u8 | 0x80).collect();
        std::fs::write(&bin, [&[0x01u8, 0x02, 0x03][..], &data].concat()).unwrap();
        assert!(!sniff_file_content(&bin).unwrap());
    }

    #[test]
    fn binary_type_comes_before_the_size_cap() {
        let dir = tempfile::tempdir().unwrap();
        let opts = ScanOptions {
            max_file_size: 1024,
            ..Default::default()
        };
        // A big asset with a binary extension is binary, not "too large".
        let bin = dir.path().join("asset.bin");
        std::fs::File::create(&bin).unwrap().set_len(4096).unwrap();
        assert!(matches!(
            classify(&bin, 4096, &opts),
            Verdict::Skip(_, SkipKind::Binary)
        ));
        // A big file of zeros with an unknown extension is sniffed as binary.
        let raw = dir.path().join("dump.dat");
        std::fs::File::create(&raw).unwrap().set_len(4096).unwrap();
        assert!(matches!(
            classify(&raw, 4096, &opts),
            Verdict::Skip(_, SkipKind::Binary)
        ));
        // Text over the cap is content that was not captured (exit 3).
        let txt = dir.path().join("notes.txt");
        std::fs::write(&txt, "word ".repeat(1000)).unwrap();
        assert!(matches!(
            classify(&txt, 5000, &opts),
            Verdict::Skip(r, SkipKind::NotCaptured) if r.starts_with("too large")
        ));
        let odd = dir.path().join("notes.xyz");
        std::fs::write(&odd, "word ".repeat(1000)).unwrap();
        assert!(matches!(
            classify(&odd, 5000, &opts),
            Verdict::Skip(r, SkipKind::NotCaptured) if r.starts_with("too large")
        ));
    }

    #[test]
    fn test_minified_extension_patterns() {
        assert!(is_minified_filename("vendor.min.js"));
        assert!(is_minified_filename("app.chunk.js"));
        assert!(is_minified_filename("VENDOR.MIN.JS"));
        assert!(!is_minified_filename("app.js"));
    }

    #[test]
    fn mount_paths_unescape_like_the_kernel_writes_them() {
        assert_eq!(
            unescape_mount_path("/media/pawel/My\\040Disk"),
            "/media/pawel/My Disk"
        );
        assert_eq!(unescape_mount_path("/mnt/a\\134b"), "/mnt/a\\b");
    }

    #[test]
    fn linux_fs_types_are_classified() {
        assert!(linux_fs_reason("proc").unwrap().contains("virtual"));
        assert!(linux_fs_reason("nfs4").unwrap().contains("network"));
        assert!(linux_fs_reason("squashfs").unwrap().contains("read-only"));
        // Real disks are scanned on a whole-drive run.
        for fs in ["ext4", "btrfs", "xfs", "vfat", "ntfs3", "fuseblk"] {
            assert_eq!(linux_fs_reason(fs), None, "{fs}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn whole_drive_areas_come_from_this_machines_mount_table() {
        let areas = system_areas(Path::new("/"));
        for p in ["/proc", "/sys"] {
            assert!(
                areas.contains_key(Path::new(p)),
                "{p} not pruned: {areas:?}"
            );
        }
        assert!(
            !areas.contains_key(Path::new("/")),
            "the root is never pruned"
        );
        // A root below the virtual mounts only prunes what lies under it.
        assert!(system_areas(Path::new("/home"))
            .keys()
            .all(|p| p.starts_with("/home")));
    }

    #[cfg(unix)]
    #[test]
    fn fifos_are_recorded_and_never_opened() {
        // Opening a FIFO for reading blocks forever: the scan must finish and
        // still account for it.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pipes");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("notes.txt"), "kept\n").unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(root.join("queue"))
            .status()
            .unwrap();
        assert!(made.success());
        let scan = scan_text_files(&root, &ScanOptions::default()).unwrap();
        assert_eq!(scan.files.len(), 1);
        let fifo = scan
            .skipped
            .iter()
            .find(|s| s.path == "queue")
            .expect("the FIFO is recorded");
        assert!(fifo.reason.contains("named pipe"), "{}", fifo.reason);
    }
}
