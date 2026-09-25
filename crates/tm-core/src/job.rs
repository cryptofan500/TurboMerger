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
}

pub fn count_not_captured(scan: &[scanner::SkipEntry], merge: &[scanner::SkipEntry]) -> usize {
    scan.iter()
        .chain(merge)
        .filter(|s| s.kind == scanner::SkipKind::NotCaptured)
        .count()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct MergeOptions {
    pub folder_path: String,
    pub output_path: Option<String>,
    pub include_venv: bool,
    pub include_tree: bool,
    pub content_detection: bool,
    #[serde(default = "default_true")]
    pub respect_gitignore: bool,
    #[serde(default)]
    pub include_hidden: bool,
    #[serde(default = "default_true")]
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
}

impl MergeOptions {
    /// The defaults every shell starts from: tree on, redaction on,
    /// `.gitignore` respected, content sniffing on.
    pub fn for_folder(folder_path: impl Into<String>) -> MergeOptions {
        MergeOptions {
            folder_path: folder_path.into(),
            output_path: None,
            include_venv: false,
            include_tree: true,
            content_detection: true,
            respect_gitignore: true,
            include_hidden: false,
            redact_secrets: true,
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
    let root = security::validate_and_canonicalize(&options.folder_path)
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

    // Output naming in one place, at merge time.
    let folder_name = root
        .file_name()
        .and_then(|n| n.to_str())
        .map(security::sanitize_filename)
        .unwrap_or_else(|| "merged".to_string());
    let timestamp = Local::now().format("%Y-%m-%dT%H-%M-%S");
    let output_name = format!(
        "{}_{}_merged.{}",
        folder_name,
        timestamp,
        format.extension()
    );
    let output_path = match &options.output_path {
        Some(p) if !p.is_empty() => {
            let pb = PathBuf::from(p);
            if pb.is_dir() {
                pb.join(&output_name)
            } else {
                pb
            }
        }
        _ => dirs::download_dir()
            .unwrap_or_else(|| root.parent().unwrap_or(&root).to_path_buf())
            .join(&output_name),
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
    };

    let merge_config = MergeConfig {
        include_tree: options.include_tree,
        redact: options.redact_secrets,
        format,
        ordering: MergeOrdering::from_str_lenient(options.ordering.as_deref().unwrap_or("path")),
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
    };

    Ok(ResolvedJob {
        root,
        output_path,
        scan_options,
        merge_config,
    })
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
) {
    for rel in force_include {
        let candidate = root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        let Ok(canon) = candidate.canonicalize() else {
            continue;
        };
        // std canonicalize yields \\?\-prefixed paths on Windows; compare
        // against the canonicalized root the same way.
        let Ok(root_canon) = root.canonicalize() else {
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
    files.sort();
}

/// Where a running job is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Walking the folder (`done` = entries seen; the total is unknown).
    Scan,
    /// Deciding what each candidate file is (`done` of `total`).
    Classify,
    /// Reading, redacting and counting files, then writing (`done` of `total`).
    Merge,
}

/// One progress report from a running job.
#[derive(Debug, Clone, Serialize)]
pub struct Progress {
    pub stage: Stage,
    pub done: usize,
    /// 0 when not known yet.
    pub total: usize,
    /// The file being worked on, when there is one.
    pub current: String,
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
    progress(Progress {
        stage: Stage::Scan,
        done: 0,
        total: 0,
        current: String::new(),
    });
    let on_scan = |p: ScanProgress| {
        progress(match p {
            ScanProgress::Walked(n) => Progress {
                stage: Stage::Scan,
                done: n,
                total: 0,
                current: String::new(),
            },
            ScanProgress::Classified { done, total } => Progress {
                stage: Stage::Classify,
                done,
                total,
                current: String::new(),
            },
        })
    };
    let scan =
        scanner::scan_with(&job.root, &job.scan_options, cancel.flag(), &on_scan).map_err(|e| {
            if e.is::<Cancelled>() {
                JobError::Cancelled
            } else {
                JobError::Scan(e.to_string())
            }
        })?;
    let stats = scan.stats;
    let mut skipped = scan.skipped;
    let mut files = scan.files;
    apply_selection(
        &job.root,
        &mut files,
        &mut skipped,
        &options.selected_paths,
        &options.force_include,
    );
    if files.is_empty() && skipped.is_empty() {
        return Err(JobError::Empty);
    }

    let ctl = crate::merger::MergeCtl {
        cancel: cancel.flag(),
        finish: Some(cancel.finish_flag()),
    };
    let outcome = crate::merger::merge_files_ctl(
        &job.root,
        &files,
        &job.output_path,
        &job.merge_config,
        ctl,
        |done, total, file| {
            progress(Progress {
                stage: Stage::Merge,
                done,
                total,
                current: file.to_string(),
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
    };
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
    fn selection_filters_and_force_include_rescues() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
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
        );
        let rels: Vec<String> = files
            .iter()
            .map(|f| crate::scanner::relative_display(&root, f))
            .collect();
        assert_eq!(rels, vec!["a.rs", "notes.txt"]);
    }
}
