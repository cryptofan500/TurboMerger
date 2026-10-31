//! A merge job as the shells describe it (`MergeOptions`), resolved against
//! the file system and `turbomerger.toml` into scan + merge settings.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use chrono::Local;

use crate::cancel::{CancelToken, Cancelled};
use crate::merger::{MergeConfig, MergeOutcome, Ordering as MergeOrdering, OutputFormat};
use crate::scanner::{self, ScanOptions, ScanProgress, ScanStats, SkipEntry};
use crate::security;

#[derive(Debug, Clone, Serialize)]
pub struct MergeResult {
    pub output_path: String,
    pub output_paths: Vec<String>,
    pub files_processed: usize,
    pub files_skipped: usize,
    pub total_bytes: usize,
    pub duration_ms: u64,
    pub files_by_extension: usize,
    pub files_by_content: usize,
    pub files_skipped_binary: usize,
    pub files_unreadable: usize,
    pub secrets_redacted: usize,
    pub tokens_o200k: usize,
    pub tokens_claude_est: usize,
    pub skill_path: Option<String>,
    /// Inputs whose content is NOT in the output (see `SkipKind::NotCaptured`).
    pub not_captured: usize,
    /// Figure crops kept beside the output (`keep_figures`).
    pub figures_kept: usize,
    /// Remarks about the output (a part over its budget, figures kept, …).
    pub notes: Vec<String>,
}

pub fn count_not_captured(scan: &[scanner::SkipEntry], merge: &[scanner::SkipEntry]) -> usize {
    scan.iter()
        .chain(merge)
        .filter(|s| s.kind == scanner::SkipKind::NotCaptured)
        .count()
}

#[derive(Debug, Clone, Deserialize)]
pub struct MergeOptions {
    pub folder_path: String,
    pub output_path: Option<String>,
    pub include_venv: bool,
    pub include_tree: bool,
    pub content_detection: bool,
    /// Leave out what `.gitignore` / `.turbomergerignore` rules hide. Off by
    /// default — ignored files are merged too (CLI `--gitignore`, desktop
    /// toggle); build output stays out by its own markers (`target/` next to
    /// `Cargo.toml`, `CACHEDIR.TAG`), venvs by theirs. The MCP server turns it on.
    #[serde(default)]
    pub respect_gitignore: bool,
    #[serde(default)]
    pub include_hidden: bool,
    /// Mask secrets in the output and leave credential files (`.env`, key
    /// material, credential dumps) out. Off by default — every text file is
    /// taken as it is (CLI `--redact`, desktop toggle); the MCP server forces
    /// it on.
    #[serde(default)]
    pub redact_secrets: bool,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub ordering: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<usize>,
    #[serde(default)]
    pub include_globs: Vec<String>,
    #[serde(default)]
    pub exclude_globs: Vec<String>,
    #[serde(default)]
    pub remove_empty_lines: bool,
    #[serde(default)]
    pub truncate_base64: bool,
    /// Signatures-only mode: elide function bodies via tree-sitter (T2-3).
    #[serde(default)]
    pub compress: bool,
    /// Remove comments via tree-sitter (T2-4).
    #[serde(default)]
    pub strip_comments: bool,
    /// Append `git diff HEAD` as a final section (T2-5).
    #[serde(default)]
    pub git_diff: bool,
    /// Append `git log -n N` as a final section; 0 = off (T2-5).
    #[serde(default)]
    pub git_log_count: usize,
    /// Write `.claude/skills/<repo>/SKILL.md` into the scanned repo (T3-4).
    #[serde(default)]
    pub emit_skill: bool,
    /// Exact relative paths to merge (curated in the file tree). None = all.
    #[serde(default)]
    pub selected_paths: Option<Vec<String>>,
    /// Relative paths rescued from scan-level skips ("include anyway").
    /// Merge-level safety (binary check, credential-dense exclusion,
    /// redaction) still applies to these.
    #[serde(default)]
    pub force_include: Vec<String>,
    /// Write the absolute source path into the output header (off: the
    /// header says `local folder "<name>"`, N-21).
    #[serde(default)]
    pub show_source_path: bool,
    /// Header label for the source (remote packs pass the repo URL).
    #[serde(default)]
    pub source_label: Option<String>,
    /// The folder is a temporary remote clone.
    #[serde(default)]
    pub remote: bool,
    /// Settings file to use instead of `<folder>/turbomerger.toml` (CLI --config).
    #[serde(default)]
    pub config_path: Option<String>,
    /// Per-file size cap in MB, overriding the config file (CLI --max-file-size).
    #[serde(default)]
    pub max_file_size_mb: Option<u64>,
    /// Same bytes on every OS (N-16).
    #[serde(default)]
    pub reproducible: bool,
    /// Read (download) cloud files that are not on this device (N-10/N-11).
    #[serde(default)]
    pub hydrate: bool,
    /// OCR image-only document pages (scanned PDFs) via the external OCR
    /// command. Off by default (CLI `--extract-images-text`).
    #[serde(default)]
    pub extract_images_text: bool,
    /// With OCR on, read thumbnails, icons, figure crops of a Markdown
    /// export and MonadDoc work folders too (CLI `--ocr-all-pictures`).
    #[serde(default)]
    pub ocr_all_pictures: bool,
    /// With OCR on, keep the figure crops monaddoc cuts out of pages beside
    /// the output (`<output stem>_assets/`) and link them from the text (CLI
    /// `--keep-figures`, desktop toggle). The MCP server never turns it on.
    #[serde(default)]
    pub keep_figures: bool,
    /// Accept a protected operating-system folder as the source
    /// (`C:\Windows`, `/usr`, `/etc`, …) for whole-computer runs (CLI
    /// `--allow-system-paths`). Never set by the MCP server.
    #[serde(default)]
    pub allow_system_paths: bool,
    /// Selection plan TOML
    #[serde(default)]
    pub selection: Option<crate::selection::SelectionPlan>,
    /// Exact relative paths to exclude from merge
    #[serde(default)]
    pub exclude_paths: Vec<String>,
    /// Exact relative directory prefixes to exclude from merge
    #[serde(default)]
    pub exclude_dirs: Vec<String>,
    /// TM-GRID/1 table chunk row interval (default 150)
    #[serde(default)]
    pub grid_chunk_rows: usize,
    /// Token budget ceiling
    #[serde(default)]
    pub budget_tokens: Option<u64>,
    /// Archive twin complete mode
    #[serde(default)]
    pub complete: bool,
    /// Embed binary files up to N MB in archive mode
    #[serde(default)]
    pub embed_binary: Option<u64>,
}

impl MergeOptions {
    /// The defaults every shell starts from: tree on, redaction off (every
    /// text file is taken), files hidden by `.gitignore` included, content
    /// sniffing on, no size cap.
    pub fn for_folder(folder_path: impl Into<String>) -> MergeOptions {
        MergeOptions {
            folder_path: folder_path.into(),
            output_path: None,
            include_venv: false,
            include_tree: true,
            content_detection: true,
            respect_gitignore: false,
            include_hidden: false,
            redact_secrets: false,
            format: None,
            ordering: None,
            max_tokens: None,
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
            remove_empty_lines: false,
            truncate_base64: false,
            compress: false,
            strip_comments: false,
            git_diff: false,
            git_log_count: 0,
            emit_skill: false,
            selected_paths: None,
            force_include: Vec::new(),
            show_source_path: false,
            source_label: None,
            remote: false,
            config_path: None,
            max_file_size_mb: None,
            reproducible: false,
            hydrate: false,
            extract_images_text: false,
            ocr_all_pictures: false,
            keep_figures: false,
            allow_system_paths: false,
            selection: None,
            exclude_paths: Vec::new(),
            exclude_dirs: Vec::new(),
            grid_chunk_rows: 150,
            budget_tokens: None,
            complete: false,
            embed_binary: None,
        }
    }
}

/// Everything needed to run a merge, resolved from UI options + config file.
pub struct ResolvedJob {
    pub root: PathBuf,
    pub output_path: PathBuf,
    pub scan_options: ScanOptions,
    pub merge_config: MergeConfig,
}

pub fn resolve_job(options: &MergeOptions) -> Result<ResolvedJob, String> {
    let root = if options.allow_system_paths {
        security::canonicalize_any_source(&options.folder_path)
    } else {
        security::validate_and_canonicalize(&options.folder_path)
    }
    .map_err(|e| format!("Security error: {}", e))?;
    if !root.exists() || !root.is_dir() {
        return Err("Invalid folder path".to_string());
    }

    let mut config = match &options.config_path {
        Some(p) => crate::config::load_from_file(Path::new(p))?,
        None => crate::config::load_from_dir(&root),
    };
    if let Some(mb) = options.max_file_size_mb {
        config.scanning.max_file_size_mb = mb;
    }
    let format = OutputFormat::from_str_lenient(options.format.as_deref().unwrap_or("markdown"));

    // Output naming in one place, at merge time (N-45). A path that ends in
    // a separator names a directory, created when missing (v7 wrote a file
    // called `out` for `out/`). Generated names never overwrite an earlier
    // output: two merges in the same second get `-2`, `-3`, ….
    let folder_name = root
        .file_name()
        .and_then(|n| n.to_str())
        .map(security::sanitize_filename)
        .unwrap_or_else(|| "merged".to_string());
    let stem = format!(
        "{}_{}",
        folder_name,
        Local::now().format("%Y-%m-%dT%H-%M-%S")
    );
    let output_path = match &options.output_path {
        Some(p) if !p.is_empty() => {
            let pb = PathBuf::from(p);
            if p.ends_with('/') || p.ends_with(std::path::MAIN_SEPARATOR) {
                std::fs::create_dir_all(&pb)
                    .map_err(|e| format!("cannot create output folder {}: {}", pb.display(), e))?;
            }
            if pb.is_dir() {
                unique_output(&pb, &stem, format.extension())
            } else {
                pb
            }
        }
        _ => unique_output(&default_output_dir(&root), &stem, format.extension()),
    };

    let mut include_globs = options.include_globs.clone();
    include_globs.extend(config.filter.include.clone());
    let mut exclude_globs = options.exclude_globs.clone();
    exclude_globs.extend(config.filter.exclude.clone());

    let scan_options = ScanOptions {
        include_venv: options.include_venv || config.scanning.include_venvs,
        content_sniff: options.content_detection && config.scanning.content_sniff,
        include_hidden: options.include_hidden || config.scanning.include_hidden,
        respect_gitignore: options.respect_gitignore,
        max_file_size: config.scanning.max_file_size_mb * 1024 * 1024,
        extra_text_exts: config.extensions.include,
        extra_skip_exts: config.extensions.exclude,
        extra_binary_exts: config.extensions.binary,
        include_globs,
        exclude_globs,
        hydrate: options.hydrate,
        protect_secrets: options.redact_secrets,
        whole_drive: options.allow_system_paths || security::is_filesystem_root(&root),
        ocr_images: options.extract_images_text,
        ocr_all_pictures: options.ocr_all_pictures,
    };

    let ordering_str = options.selection.as_ref().and_then(|s| s.ordering.as_deref()).or(options.ordering.as_deref()).unwrap_or("path");
    let ordering = MergeOrdering::from_str_lenient(ordering_str);

    let merge_config = MergeConfig {
        include_tree: options.include_tree,
        redact: options.redact_secrets,
        format,
        ordering,
        max_tokens: options.max_tokens.filter(|&t| t > 0),
        remove_empty_lines: options.remove_empty_lines,
        truncate_base64: options.truncate_base64,
        compress: options.compress,
        strip_comments: options.strip_comments,
        git_diff: options.git_diff,
        git_log: options.git_log_count,
        emit_skill: options.emit_skill,
        source_label: options.source_label.clone(),
        show_source_path: options.show_source_path,
        remote: options.remote,
        reproducible: options.reproducible,
        extract_images_text: options.extract_images_text,
        keep_figures: options.keep_figures && options.extract_images_text,
        max_file_size: config.scanning.max_file_size_mb * 1024 * 1024,
        selection: options.selection.clone(),
        grid_chunk_rows: if options.grid_chunk_rows == 0 { 150 } else { options.grid_chunk_rows },
        complete: options.complete,
        embed_binary: options.embed_binary,
        budget_tokens: options.budget_tokens,
    };

    Ok(ResolvedJob {
        root,
        output_path,
        scan_options,
        merge_config,
    })
}

/// Where outputs go when none is named: Downloads, else the home folder,
/// else the working directory (else next to the source).
fn default_output_dir(root: &Path) -> PathBuf {
    dirs::download_dir()
        .filter(|d| d.is_dir())
        .or_else(|| dirs::home_dir().filter(|d| d.is_dir()))
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| root.parent().unwrap_or(root).to_path_buf())
}

/// `<dir>/<stem>_merged.<ext>`, or `<stem>-2_merged.<ext>`, `-3`, … when
/// that name (or a split part of it) exists. The counter sits before
/// `_merged` so the scanner still recognises the file as an output.
fn unique_output(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let taken = |s: &str| {
        dir.join(format!("{}_merged.{}", s, ext)).exists()
            || std::fs::read_dir(dir).is_ok_and(|rd| {
                let prefix = format!("{}_merged.part", s);
                rd.flatten()
                    .any(|e| e.file_name().to_string_lossy().starts_with(&prefix))
            })
    };
    let mut candidate = stem.to_string();
    let mut n = 1;
    while taken(&candidate) {
        n += 1;
        candidate = format!("{}-{}", stem, n);
    }
    dir.join(format!("{}_merged.{}", candidate, ext))
}

/// Apply force-include rescues and the curated selection to a scan result.
/// Force-included paths are validated to stay inside the root; selection is
/// an exact relative-path filter.
pub fn apply_selection(
    root: &Path,
    files: &mut Vec<PathBuf>,
    skipped: &mut Vec<scanner::SkipEntry>,
    selected_paths: &Option<Vec<String>>,
    force_include: &[String],
    exclude_paths: &[String],
    exclude_dirs: &[String],
    keep_order: bool,
) {
    for rel in force_include {
        let candidate = root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        let Ok(canon) = dunce::canonicalize(&candidate) else {
            continue;
        };
        // std canonicalize yields \\?\-prefixed paths on Windows; compare
        // against the canonicalized root the same way.
        let Ok(root_canon) = dunce::canonicalize(root) else {
            continue;
        };
        if !canon.starts_with(&root_canon) || !candidate.is_file() {
            continue;
        }
        if !files.contains(&candidate) {
            files.push(candidate);
            skipped.retain(|s| s.path != *rel);
        }
    }
    if let Some(sel) = selected_paths {
        let want: std::collections::HashSet<&str> = sel.iter().map(|s| s.as_str()).collect();
        files.retain(|f| want.contains(scanner::relative_display(root, f).as_str()));
    }
    if !exclude_paths.is_empty() {
        let unpick: std::collections::HashSet<&str> = exclude_paths.iter().map(|s| s.as_str()).collect();
        files.retain(|f| !unpick.contains(scanner::relative_display(root, f).as_str()));
    }
    if !exclude_dirs.is_empty() {
        files.retain(|f| {
            let rel = scanner::relative_display(root, f);
            !exclude_dirs.iter().any(|d| {
                let clean = d.trim_end_matches('/');
                rel == clean || rel.starts_with(&format!("{clean}/"))
            })
        });
    }
    if !keep_order {
        files.sort();
    }
}

/// Where a running job is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Walking the folder (`done` = entries seen; the total is unknown).
    #[default]
    Scan,
    /// Deciding what each candidate file is (`done` of `total`).
    Classify,
    /// Reading, redacting and counting files, then writing (`done` of `total`).
    Merge,
    /// Reading text from pictures (`done` of `total` pages), after the text.
    Ocr,
}

/// One progress report from a running job.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Progress {
    pub stage: Stage,
    pub done: usize,
    /// 0 when not known yet.
    pub total: usize,
    /// The file being worked on, when there is one.
    pub current: String,
    /// Merge stage: input bytes finished, and in all (0 = not known). The
    /// ETA weighs files by size: a thousand notes and one big PDF are not
    /// a thousand and one equal steps.
    pub bytes_done: u64,
    pub bytes_total: u64,
    /// Seconds of work queued for a later stage (pictures waiting for the
    /// OCR stage), added to this stage's ETA.
    pub later_secs: u64,
}

impl Progress {
    /// A report with no byte counts and nothing queued for later.
    pub fn new(stage: Stage, done: usize, total: usize, current: impl Into<String>) -> Progress {
        Progress {
            stage,
            done,
            total,
            current: current.into(),
            ..Progress::default()
        }
    }
}

/// Why a job produced no output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobError {
    /// Bad options or source (message from `resolve_job`).
    Invalid(String),
    Scan(String),
    Merge(String),
    /// The folder holds nothing at all (no file, and nothing skipped).
    Empty,
    /// The job's cancel token was set; nothing was written.
    Cancelled,
}

impl std::fmt::Display for JobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobError::Invalid(m) => f.write_str(m),
            JobError::Scan(m) => write!(f, "Scan failed: {}", m),
            JobError::Merge(m) => write!(f, "Merge failed: {}", m),
            JobError::Empty => f.write_str("No files found in directory"),
            JobError::Cancelled => f.write_str("Operation cancelled by user"),
        }
    }
}

impl std::error::Error for JobError {}

/// A finished merge: the summary the shells show, plus the details.
pub struct MergeRun {
    pub result: MergeResult,
    pub outcome: MergeOutcome,
    /// What the scan left out (the merge's own skips are in `outcome`).
    pub scan_skipped: Vec<SkipEntry>,
    pub stats: ScanStats,
}

impl MergeRun {
    /// Every skip, scan first.
    pub fn all_skips(&self) -> impl Iterator<Item = &SkipEntry> {
        self.scan_skipped.iter().chain(self.outcome.skipped.iter())
    }
}

/// Resolve, scan, curate and merge one job — the path every shell (CLI,
/// MCP, desktop, watch mode) takes. `output` overrides the resolved output
/// path (watch mode writes to a stable name). Checks `cancel` per file.
pub fn run_merge(
    options: &MergeOptions,
    output: Option<&Path>,
    cancel: &CancelToken,
    progress: &(dyn Fn(Progress) + Sync),
) -> Result<MergeRun, JobError> {
    let start = std::time::Instant::now();
    let mut job = resolve_job(options).map_err(JobError::Invalid)?;
    if let Some(o) = output {
        job.output_path = o.to_path_buf();
    }
    progress(Progress::new(Stage::Scan, 0, 0, ""));
    let on_scan = |p: ScanProgress| {
        progress(match p {
            ScanProgress::Walked(n) => Progress::new(Stage::Scan, n, 0, ""),
            ScanProgress::Classified { done, total } => {
                Progress::new(Stage::Classify, done, total, "")
            }
        })
    };
    let scan = if let Some(plan) = &options.selection {
        if plan.mode == crate::selection::SelectionMode::Explicit {
            let paths: Vec<String> = plan.files.iter().filter(|f| !f.exclude).map(|f| f.path.clone()).collect();
            scanner::scan_explicit(&job.root, &paths, &job.scan_options).map_err(|e| JobError::Scan(e.to_string()))?
        } else {
            scanner::scan_with(&job.root, &job.scan_options, cancel.flag(), &on_scan).map_err(|e| {
                if e.is::<Cancelled>() {
                    JobError::Cancelled
                } else {
                    JobError::Scan(e.to_string())
                }
            })?
        }
    } else {
        scanner::scan_with(&job.root, &job.scan_options, cancel.flag(), &on_scan).map_err(|e| {
            if e.is::<Cancelled>() {
                JobError::Cancelled
            } else {
                JobError::Scan(e.to_string())
            }
        })?
    };
    let stats = scan.stats;
    let mut skipped = scan.skipped;
    let mut files = scan.files;
    let keep_order = job.merge_config.ordering == MergeOrdering::Plan;
    apply_selection(
        &job.root,
        &mut files,
        &mut skipped,
        &options.selected_paths,
        &options.force_include,
        &options.exclude_paths,
        &options.exclude_dirs,
        keep_order,
    );
    if files.is_empty() && skipped.is_empty() {
        return Err(JobError::Empty);
    }

    let watch = crate::merger::MergeWatch::default();
    let on_ocr = |done: usize, total: usize, current: &str| {
        progress(Progress::new(Stage::Ocr, done, total, current))
    };
    let ctl = crate::merger::MergeCtl {
        cancel: cancel.flag(),
        finish: Some(cancel.finish_flag()),
        watch: Some(&watch),
        on_ocr: Some(&on_ocr),
    };
    let outcome = crate::merger::merge_files_ctl(
        &job.root,
        &files,
        &job.output_path,
        &job.merge_config,
        ctl,
        |done, total, file| {
            use std::sync::atomic::Ordering::Relaxed;
            progress(Progress {
                bytes_done: watch.bytes_done.load(Relaxed),
                bytes_total: watch.bytes_total.load(Relaxed),
                later_secs: watch.ocr_secs.load(Relaxed),
                ..Progress::new(Stage::Merge, done, total, file)
            })
        },
        &skipped,
    )
    .map_err(|e| {
        if e.is::<Cancelled>() {
            JobError::Cancelled
        } else {
            JobError::Merge(e.to_string())
        }
    })?;

    let result = MergeResult {
        output_path: outcome
            .outputs
            .first()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default(),
        output_paths: outcome
            .outputs
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect(),
        files_processed: outcome.files_processed,
        files_skipped: outcome.files_skipped + skipped.len(),
        total_bytes: outcome.total_bytes,
        duration_ms: start.elapsed().as_millis() as u64,
        files_by_extension: stats.by_extension,
        files_by_content: stats.by_content,
        files_skipped_binary: stats.skipped_binary,
        files_unreadable: stats.unreadable,
        secrets_redacted: outcome.secrets_redacted,
        tokens_o200k: outcome.tokens_o200k,
        tokens_claude_est: crate::tokens::claude_estimate(outcome.tokens_o200k),
        skill_path: outcome
            .skill
            .as_ref()
            .map(|p| p.to_string_lossy().to_string()),
        not_captured: count_not_captured(&skipped, &outcome.skipped),
        figures_kept: outcome.figures_kept,
        notes: outcome.notes.clone(),
    };
    // Best effort: a cache that cannot be written only costs time next run.
    let _ = crate::cache::save();
    Ok(MergeRun {
        result,
        outcome,
        scan_skipped: skipped,
        stats,
    })
}

/// A source the shells were given, ready to scan.
pub struct ResolvedSource {
    /// The folder to merge (a temporary clone for remote sources).
    pub root: String,
    /// Keeps a remote clone alive; dropping it deletes the clone.
    pub checkout: Option<crate::remote::RemoteCheckout>,
    /// The repository URL, for remote sources.
    pub remote_label: Option<String>,
}

/// A local path, or an explicit remote reference (`https://…`, `git@…`,
/// `gh:owner/repo`) cloned into a temporary directory. `on_clone` is told
/// the URL before a clone starts (the shells print it).
pub fn resolve_source(src: &str, on_clone: impl FnOnce(&str)) -> Result<ResolvedSource, String> {
    if Path::new(src).exists() {
        return Ok(ResolvedSource {
            root: src.to_string(),
            checkout: None,
            remote_label: None,
        });
    }
    if let Some((url, name)) = crate::remote::parse_remote_explicit(src) {
        let pat = std::env::var("TURBOMERGER_PAT").ok();
        on_clone(&url);
        let co = crate::remote::clone_shallow(&url, &name, pat.as_deref())?;
        let root = co.path.to_string_lossy().to_string();
        return Ok(ResolvedSource {
            root,
            checkout: Some(co),
            remote_label: Some(url),
        });
    }
    if crate::remote::parse_remote(src).is_some() {
        return Err(format!(
            "source not found: {} (to pack the GitHub repository, use gh:{})",
            src, src
        ));
    }
    Err(format!("source not found: {}", src))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_merge_reports_progress_and_honours_cancel() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("r");
        std::fs::create_dir_all(root.join("src")).unwrap();
        for i in 0..40 {
            std::fs::write(
                root.join(format!("src/f{i}.rs")),
                format!("fn f{i}() {{}}\n"),
            )
            .unwrap();
        }
        let out = tmp.path().join("out.md");
        let options = MergeOptions::for_folder(root.to_string_lossy());

        let seen = std::sync::Mutex::new(Vec::new());
        let run = run_merge(&options, Some(&out), &CancelToken::new(), &|p| {
            seen.lock().unwrap().push(p.stage)
        })
        .unwrap();
        assert_eq!(run.result.files_processed, 40);
        assert!(out.is_file());
        let stages = seen.into_inner().unwrap();
        assert_eq!(stages.first(), Some(&Stage::Scan));
        assert!(stages.contains(&Stage::Classify) && stages.contains(&Stage::Merge));

        // Cancelled mid-merge: an error, and no output.
        let out2 = tmp.path().join("out2.md");
        let cancel = CancelToken::new();
        let err = run_merge(&options, Some(&out2), &cancel, &|p| {
            if p.stage == Stage::Merge {
                cancel.cancel();
            }
        })
        .err();
        assert_eq!(err, Some(JobError::Cancelled));
        assert!(!out2.exists());

        // Empty folder.
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let err = run_merge(
            &MergeOptions::for_folder(empty.to_string_lossy()),
            Some(&out2),
            &CancelToken::new(),
            &|_| {},
        )
        .err();
        assert_eq!(err, Some(JobError::Empty));
    }

    #[test]
    fn generated_names_never_overwrite_and_trailing_slash_makes_a_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        assert_eq!(unique_output(dir, "r_t", "md"), dir.join("r_t_merged.md"));
        std::fs::write(dir.join("r_t_merged.md"), "x").unwrap();
        assert_eq!(unique_output(dir, "r_t", "md"), dir.join("r_t-2_merged.md"));
        std::fs::write(dir.join("r_t-2_merged.part1-of-3.md"), "x").unwrap();
        assert_eq!(unique_output(dir, "r_t", "md"), dir.join("r_t-3_merged.md"));
        assert!(crate::scanner::is_own_output_name("r_t-3_merged.md"));

        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mut o = MergeOptions::for_folder(src.to_string_lossy());
        let out = format!("{}/new/", dir.display());
        o.output_path = Some(out.clone());
        let job = resolve_job(&o).unwrap();
        assert!(dir.join("new").is_dir(), "created");
        assert_eq!(job.output_path.parent(), Some(dir.join("new").as_path()));
    }

    #[test]
    fn selection_filters_and_force_include_rescues() {
        let tmp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(tmp.path()).unwrap();
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(root.join("b.rs"), "fn b() {}\n").unwrap();
        std::fs::write(root.join("notes.txt"), "hello\n").unwrap();

        let mut files = vec![root.join("a.rs"), root.join("b.rs")];
        let mut skipped = vec![crate::scanner::SkipEntry::new(
            "notes.txt",
            "test skip",
            crate::scanner::SkipKind::Excluded,
        )];

        // Force-include rescues the skipped file and clears its skip entry.
        apply_selection(
            &root,
            &mut files,
            &mut skipped,
            &None,
            &["notes.txt".to_string(), "../escape.txt".to_string()],
            &[],
            &[],
            false,
        );
        assert!(files.iter().any(|f| f.ends_with("notes.txt")));
        assert!(skipped.is_empty());
        assert_eq!(files.len(), 3, "path traversal must not add files");

        // Selection keeps exactly the named subset.
        apply_selection(
            &root,
            &mut files,
            &mut skipped,
            &Some(vec!["a.rs".to_string(), "notes.txt".to_string()]),
            &[],
            &[],
            &[],
            false,
        );
        let rels: Vec<String> = files
            .iter()
            .map(|f| crate::scanner::relative_display(&root, f))
            .collect();
        assert_eq!(rels, vec!["a.rs", "notes.txt"]);
    }
}
