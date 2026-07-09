//! Tauri command handlers + a headless CLI path (shared merge core).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

use chrono::Local;
use tauri::{AppHandle, Emitter, State};

use crate::merger::{MergeConfig, Ordering as MergeOrdering, OutputFormat};
use crate::scanner::{self, ScanOptions};
use crate::security;

/// Application state for cancellation
pub struct AppState {
    pub cancel_flag: Arc<AtomicBool>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            cancel_flag: Arc::new(AtomicBool::new(false)),
        }
    }
}

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
}

#[derive(Debug, Serialize, Clone)]
pub struct ProgressUpdate {
    pub current: usize,
    pub total: usize,
    pub current_file: String,
    pub percentage: f32,
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
}

/// Everything needed to run a merge, resolved from UI options + config file.
pub(crate) struct ResolvedJob {
    pub(crate) root: PathBuf,
    pub(crate) output_path: PathBuf,
    pub(crate) scan_options: ScanOptions,
    pub(crate) merge_config: MergeConfig,
}

pub(crate) fn resolve_job(options: &MergeOptions) -> Result<ResolvedJob, String> {
    let root = security::validate_and_canonicalize(&options.folder_path)
        .map_err(|e| format!("Security error: {}", e))?;
    if !root.exists() || !root.is_dir() {
        return Err("Invalid folder path".to_string());
    }

    let config = crate::config::load_from_dir(&root);
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
fn apply_selection(
    root: &std::path::Path,
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

#[derive(Debug, Serialize)]
pub struct ScanEntry {
    pub path: String,
    pub size: u64,
    pub tokens: usize,
}

#[derive(Debug, Serialize)]
pub struct ScanReport {
    pub root: String,
    pub included: Vec<ScanEntry>,
    pub skipped: Vec<scanner::SkipEntry>,
    pub total_tokens: usize,
    pub duration_ms: u64,
}

/// Scan without merging: per-file sizes + o200k token counts feed the curate
/// tree, the treemap, and the skip drill-in. Token counts are on raw content
/// (pre-redaction/slimming) — close enough for curation.
#[tauri::command]
pub async fn scan_folder(app: AppHandle, options: MergeOptions) -> Result<ScanReport, String> {
    let start = std::time::Instant::now();
    let job = resolve_job(&options)?;

    let _ = app.emit(
        "scan-progress",
        ProgressUpdate {
            current: 0,
            total: 0,
            current_file: "Scanning directory...".to_string(),
            percentage: 0.0,
        },
    );

    let scan = scanner::scan_text_files(&job.root, &job.scan_options)
        .map_err(|e| format!("Scan failed: {}", e))?;

    use rayon::prelude::*;
    let total = scan.files.len();
    let counter = std::sync::atomic::AtomicUsize::new(0);
    let mut included: Vec<ScanEntry> = scan
        .files
        .par_iter()
        .map(|path| {
            let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            let tokens = std::fs::read(path)
                .map(|bytes| crate::tokens::count(&String::from_utf8_lossy(&bytes)))
                .unwrap_or(0);
            let done = counter.fetch_add(1, Ordering::Relaxed) + 1;
            if done.is_multiple_of(32) || done == total {
                let _ = app.emit(
                    "scan-progress",
                    ProgressUpdate {
                        current: done,
                        total,
                        current_file: scanner::relative_display(&job.root, path),
                        percentage: (done as f32 / total as f32) * 100.0,
                    },
                );
            }
            ScanEntry {
                path: scanner::relative_display(&job.root, path),
                size,
                tokens,
            }
        })
        .collect();
    included.sort_by(|a, b| a.path.cmp(&b.path));
    let total_tokens = included.iter().map(|e| e.tokens).sum();

    Ok(ScanReport {
        root: job.root.to_string_lossy().to_string(),
        included,
        skipped: scan.skipped,
        total_tokens,
        duration_ms: start.elapsed().as_millis() as u64,
    })
}

/// Build an aider-style repo map (tags → PageRank → budgeted signatures).
#[tauri::command]
pub async fn repo_map(options: MergeOptions, token_budget: Option<usize>) -> Result<String, String> {
    let job = resolve_job(&options)?;
    let scan = scanner::scan_text_files(&job.root, &job.scan_options)
        .map_err(|e| format!("Scan failed: {}", e))?;
    Ok(crate::repomap::build_repo_map(
        &job.root,
        &scan.files,
        token_budget.unwrap_or(1024),
    ))
}

#[tauri::command]
pub fn get_downloads_path() -> Result<String, String> {
    dirs::download_dir()
        .map(|p| p.to_string_lossy().to_string())
        .ok_or_else(|| "Could not find Downloads folder".to_string())
}

#[tauri::command]
pub fn cancel_merge(state: State<'_, Mutex<AppState>>) -> Result<(), String> {
    state
        .lock()
        .map_err(|e| e.to_string())?
        .cancel_flag
        .store(true, Ordering::Relaxed);
    Ok(())
}

#[tauri::command]
pub fn reset_cancel(state: State<'_, Mutex<AppState>>) -> Result<(), String> {
    state
        .lock()
        .map_err(|e| e.to_string())?
        .cancel_flag
        .store(false, Ordering::Relaxed);
    Ok(())
}

#[tauri::command]
pub async fn merge_folder(
    app: AppHandle,
    state: State<'_, Mutex<AppState>>,
    options: MergeOptions,
) -> Result<MergeResult, String> {
    let cancel_flag = {
        let state = state.lock().map_err(|e| e.to_string())?;
        state.cancel_flag.clone()
    };
    cancel_flag.store(false, Ordering::Relaxed);
    do_merge(&app, &cancel_flag, &options)
}

/// Pack a remote repo: shallow-clone (temp, self-cleaning) then run the
/// normal merge pipeline over the checkout. `pat` stays in memory only.
#[tauri::command]
pub async fn pack_remote(
    app: AppHandle,
    state: State<'_, Mutex<AppState>>,
    url: String,
    pat: Option<String>,
    options: MergeOptions,
) -> Result<MergeResult, String> {
    let (clone_url, name) = crate::remote::parse_remote(&url)
        .ok_or("Not a recognizable repo reference (URL or owner/repo)")?;
    let _ = app.emit(
        "merge-progress",
        ProgressUpdate {
            current: 0,
            total: 0,
            current_file: format!("Cloning {} (shallow)...", clone_url),
            percentage: 0.0,
        },
    );
    let checkout = crate::remote::clone_shallow(&clone_url, &name, pat.as_deref())?;

    let cancel_flag = {
        let state = state.lock().map_err(|e| e.to_string())?;
        state.cancel_flag.clone()
    };
    cancel_flag.store(false, Ordering::Relaxed);

    let mut opts = options;
    opts.folder_path = checkout.path.to_string_lossy().to_string();
    do_merge(&app, &cancel_flag, &opts)
    // checkout drops here: temp clone deleted.
}

/// The shared merge core: scan → curate → merge with progress events.
fn do_merge(
    app: &AppHandle,
    cancel_flag: &std::sync::atomic::AtomicBool,
    options: &MergeOptions,
) -> Result<MergeResult, String> {
    let start = std::time::Instant::now();
    let job = resolve_job(options)?;

    let _ = app.emit(
        "merge-progress",
        ProgressUpdate {
            current: 0,
            total: 0,
            current_file: "Scanning directory...".to_string(),
            percentage: 0.0,
        },
    );

    let scan = scanner::scan_text_files(&job.root, &job.scan_options)
        .map_err(|e| format!("Scan failed: {}", e))?;
    let scan_stats = scan.stats;
    let mut scan_skips = scan.skipped;
    let mut files = scan.files;
    apply_selection(
        &job.root,
        &mut files,
        &mut scan_skips,
        &options.selected_paths,
        &options.force_include,
    );
    if files.is_empty() {
        return Err("No text files found in directory".to_string());
    }

    let mut cfg = job.merge_config;
    cfg.include_tree = cfg.include_tree && files.len() < 50_000;

    let outcome = crate::merger::merge_files_with_progress(
        &job.root,
        &files,
        &job.output_path,
        &cfg,
        cancel_flag,
        |current, total, file_name| {
            let _ = app.emit(
                "merge-progress",
                ProgressUpdate {
                    current,
                    total,
                    current_file: file_name.to_string(),
                    percentage: (current as f32 / total as f32) * 100.0,
                },
            );
        },
        &scan_skips,
    )
    .map_err(|e| format!("Merge failed: {}", e))?;

    if cancel_flag.load(Ordering::Relaxed) {
        for p in &outcome.outputs {
            let _ = std::fs::remove_file(p);
        }
        return Err("Operation cancelled by user".to_string());
    }

    Ok(MergeResult {
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
        files_skipped: outcome.files_skipped + scan_skips.len(),
        total_bytes: outcome.total_bytes,
        duration_ms: start.elapsed().as_millis() as u64,
        files_by_extension: scan_stats.by_extension,
        files_by_content: scan_stats.by_content,
        files_skipped_binary: scan_stats.skipped_binary,
        files_unreadable: scan_stats.unreadable,
        secrets_redacted: outcome.secrets_redacted,
        tokens_o200k: outcome.tokens_o200k,
        tokens_claude_est: crate::tokens::claude_estimate(outcome.tokens_o200k),
        skill_path: outcome
            .skill
            .as_ref()
            .map(|p| p.to_string_lossy().to_string()),
    })
}

// ============================================================================
// WATCH MODE (T2-6)
// ============================================================================

/// Holds the live watcher; dropping the debouncer stops watching.
#[derive(Default)]
pub struct WatchState {
    debouncer: Option<notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>>,
}

/// Filesystem events that must NOT retrigger a watch merge: anything under
/// .git (index churn on every git command) and our own outputs.
fn watch_event_is_relevant(path: &std::path::Path) -> bool {
    if path.components().any(|c| c.as_os_str() == ".git") {
        return false;
    }
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        if name.to_ascii_lowercase().contains("_merged.") {
            return false;
        }
    }
    true
}

/// One watch-triggered merge to a fixed output path (no timestamp — the
/// point is a stable file that overwrites in place).
fn run_watch_merge(options: &MergeOptions, output: &std::path::Path) -> Result<MergeResult, String> {
    let start = std::time::Instant::now();
    let job = resolve_job(options)?;
    let scan = scanner::scan_text_files(&job.root, &job.scan_options)
        .map_err(|e| format!("Scan failed: {}", e))?;
    let scan_stats = scan.stats;
    let mut scan_skips = scan.skipped;
    let mut files = scan.files;
    apply_selection(
        &job.root,
        &mut files,
        &mut scan_skips,
        &options.selected_paths,
        &options.force_include,
    );
    if files.is_empty() {
        return Err("No text files found in directory".to_string());
    }
    let cancel = AtomicBool::new(false);
    let outcome = crate::merger::merge_files_with_progress(
        &job.root,
        &files,
        output,
        &job.merge_config,
        &cancel,
        |_, _, _| {},
        &scan_skips,
    )
    .map_err(|e| format!("Merge failed: {}", e))?;
    Ok(MergeResult {
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
        files_skipped: outcome.files_skipped + scan_skips.len(),
        total_bytes: outcome.total_bytes,
        duration_ms: start.elapsed().as_millis() as u64,
        files_by_extension: scan_stats.by_extension,
        files_by_content: scan_stats.by_content,
        files_skipped_binary: scan_stats.skipped_binary,
        files_unreadable: scan_stats.unreadable,
        secrets_redacted: outcome.secrets_redacted,
        tokens_o200k: outcome.tokens_o200k,
        tokens_claude_est: crate::tokens::claude_estimate(outcome.tokens_o200k),
        skill_path: outcome
            .skill
            .as_ref()
            .map(|p| p.to_string_lossy().to_string()),
    })
}

fn emit_watch_result(app: &AppHandle, result: &Result<MergeResult, String>) {
    match result {
        Ok(r) => {
            let _ = app.emit("watch-merged", r.clone());
        }
        Err(e) => {
            let _ = app.emit("watch-error", e.clone());
        }
    }
}

/// Start watching `options.folder_path`; re-merge (debounced 300 ms) on
/// changes. Returns the stable output path.
#[tauri::command]
pub fn start_watch(
    app: AppHandle,
    watch: State<'_, Mutex<WatchState>>,
    options: MergeOptions,
) -> Result<String, String> {
    let job = resolve_job(&options)?;
    let folder_name = job
        .root
        .file_name()
        .and_then(|n| n.to_str())
        .map(security::sanitize_filename)
        .unwrap_or_else(|| "merged".to_string());
    let stable_name = format!(
        "{}_watch_merged.{}",
        folder_name,
        job.merge_config.format.extension()
    );
    let output = job.output_path.with_file_name(stable_name);
    let root = job.root.clone();

    // Merge once up front so the output exists before the first change.
    let first = run_watch_merge(&options, &output);
    emit_watch_result(&app, &first);
    first?;

    let app2 = app.clone();
    let opts2 = options.clone();
    let out2 = output.clone();
    let busy = Arc::new(Mutex::new(()));
    let mut debouncer = notify_debouncer_mini::new_debouncer(
        std::time::Duration::from_millis(300),
        move |res: notify_debouncer_mini::DebounceEventResult| match res {
            Ok(events) => {
                if !events.iter().any(|e| watch_event_is_relevant(&e.path)) {
                    return;
                }
                // A merge already running: skip; the next change re-fires.
                let Ok(_guard) = busy.try_lock() else { return };
                let result = run_watch_merge(&opts2, &out2);
                emit_watch_result(&app2, &result);
            }
            Err(e) => {
                let _ = app2.emit("watch-error", format!("watch error: {}", e));
            }
        },
    )
    .map_err(|e| format!("watcher init failed: {}", e))?;
    debouncer
        .watcher()
        .watch(&root, notify::RecursiveMode::Recursive)
        .map_err(|e| format!("watch failed: {}", e))?;

    watch.lock().map_err(|e| e.to_string())?.debouncer = Some(debouncer);
    Ok(output.to_string_lossy().to_string())
}

#[tauri::command]
pub fn stop_watch(watch: State<'_, Mutex<WatchState>>) -> Result<(), String> {
    // Dropping the debouncer shuts the watcher thread down.
    watch.lock().map_err(|e| e.to_string())?.debouncer.take();
    Ok(())
}

#[tauri::command]
pub fn open_file(path: String) -> Result<(), String> {
    open::that(&path).map_err(|e| format!("Failed to open file: {}", e))
}

/// Reveal a file in Explorer (selects it rather than opening the parent blindly)
#[tauri::command]
pub fn open_folder(path: String) -> Result<(), String> {
    #[cfg(windows)]
    {
        std::process::Command::new("explorer.exe")
            .arg(format!("/select,{}", path))
            .spawn()
            .map_err(|e| format!("Failed to open folder: {}", e))?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let pb = PathBuf::from(&path);
        let folder = pb.parent().unwrap_or(&pb);
        open::that(folder).map_err(|e| format!("Failed to open folder: {}", e))
    }
}

// ============================================================================
// HEADLESS CLI  —  turbomerger merge <src> [out] [--flags]
// ============================================================================

pub struct CliArgs {
    pub src: String,
    pub out: Option<String>,
    pub format: Option<String>,
    pub ordering: Option<String>,
    pub max_tokens: Option<usize>,
    pub no_redact: bool,
    pub no_gitignore: bool,
    pub include_hidden: bool,
    pub include_venv: bool,
    pub remove_empty_lines: bool,
    pub truncate_base64: bool,
    pub compress: bool,
    pub strip_comments: bool,
    pub git_diff: bool,
    pub git_log: usize,
    pub emit_skill: bool,
    pub include_globs: Vec<String>,
    pub exclude_globs: Vec<String>,
    pub quiet: bool,
}

impl CliArgs {
    /// Parse `merge <src> [out] [--flags]`. Returns None if argv isn't a CLI run.
    pub fn parse(argv: &[String]) -> Option<CliArgs> {
        let mut it = argv.iter().skip(1);
        if it.next().map(|s| s.as_str()) != Some("merge") {
            return None;
        }
        let mut a = CliArgs {
            src: String::new(),
            out: None,
            format: None,
            ordering: None,
            max_tokens: None,
            no_redact: false,
            no_gitignore: false,
            include_hidden: false,
            include_venv: false,
            remove_empty_lines: false,
            truncate_base64: false,
            compress: false,
            strip_comments: false,
            git_diff: false,
            git_log: 0,
            emit_skill: false,
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
            quiet: false,
        };
        let mut positionals: Vec<String> = Vec::new();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--format" => a.format = it.next().cloned(),
                "--ordering" => a.ordering = it.next().cloned(),
                "--max-tokens" => a.max_tokens = it.next().and_then(|s| s.parse().ok()),
                "--include" => {
                    if let Some(g) = it.next() {
                        a.include_globs.push(g.clone());
                    }
                }
                "--exclude" => {
                    if let Some(g) = it.next() {
                        a.exclude_globs.push(g.clone());
                    }
                }
                "--no-redact" => a.no_redact = true,
                "--no-gitignore" => a.no_gitignore = true,
                "--include-hidden" => a.include_hidden = true,
                "--include-venv" => a.include_venv = true,
                "--remove-empty-lines" => a.remove_empty_lines = true,
                "--truncate-base64" => a.truncate_base64 = true,
                "--compress" => a.compress = true,
                "--strip-comments" => a.strip_comments = true,
                "--git-diff" => a.git_diff = true,
                "--git-log" => a.git_log = it.next().and_then(|s| s.parse().ok()).unwrap_or(10),
                "--emit-skill" => a.emit_skill = true,
                "--quiet" | "-q" => a.quiet = true,
                other => positionals.push(other.to_string()),
            }
        }
        if positionals.is_empty() {
            eprintln!("usage: turbomerger merge <src_dir> [out] [--format md|xml|cxml|json|plain] [--ordering path|entry-first|important-last] [--max-tokens N] [--include GLOB] [--exclude GLOB] [--no-redact] [--no-gitignore] [--include-hidden] [--include-venv] [--remove-empty-lines] [--truncate-base64] [--compress] [--strip-comments] [--git-diff] [--git-log N] [--emit-skill]");
            return Some(a); // src empty -> run_cli reports error
        }
        a.src = positionals[0].clone();
        a.out = positionals.get(1).cloned();
        Some(a)
    }
}

// ============================================================================
// HEADLESS CLI  —  turbomerger map <src> [out] [--tokens N] [--flags]
// ============================================================================

pub struct MapArgs {
    pub src: String,
    pub out: Option<String>,
    pub tokens: usize,
    pub no_gitignore: bool,
    pub include_hidden: bool,
    pub include_venv: bool,
    pub include_globs: Vec<String>,
    pub exclude_globs: Vec<String>,
}

impl MapArgs {
    /// Parse `map <src> [out] [--flags]`. Returns None if argv isn't a map run.
    pub fn parse(argv: &[String]) -> Option<MapArgs> {
        let mut it = argv.iter().skip(1);
        if it.next().map(|s| s.as_str()) != Some("map") {
            return None;
        }
        let mut a = MapArgs {
            src: String::new(),
            out: None,
            tokens: 1024,
            no_gitignore: false,
            include_hidden: false,
            include_venv: false,
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
        };
        let mut positionals: Vec<String> = Vec::new();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--tokens" | "--max-tokens" => {
                    if let Some(t) = it.next().and_then(|s| s.parse().ok()) {
                        a.tokens = t;
                    }
                }
                "--include" => {
                    if let Some(g) = it.next() {
                        a.include_globs.push(g.clone());
                    }
                }
                "--exclude" => {
                    if let Some(g) = it.next() {
                        a.exclude_globs.push(g.clone());
                    }
                }
                "--no-gitignore" => a.no_gitignore = true,
                "--include-hidden" => a.include_hidden = true,
                "--include-venv" => a.include_venv = true,
                other => positionals.push(other.to_string()),
            }
        }
        if positionals.is_empty() {
            eprintln!("usage: turbomerger map <src_dir> [out] [--tokens N] [--include GLOB] [--exclude GLOB] [--no-gitignore] [--include-hidden] [--include-venv]");
            return Some(a); // src empty -> run_map_cli reports error
        }
        a.src = positionals[0].clone();
        a.out = positionals.get(1).cloned();
        Some(a)
    }
}

/// Run a headless repo-map. Prints to stdout (or writes `out`). Exit code.
pub fn run_map_cli(a: MapArgs) -> i32 {
    if a.src.is_empty() {
        return 2;
    }
    let (src_root, _checkout) = match resolve_cli_source(&a.src) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };
    let options = MergeOptions {
        folder_path: src_root,
        output_path: None,
        include_venv: a.include_venv,
        include_tree: false,
        content_detection: true,
        respect_gitignore: !a.no_gitignore,
        include_hidden: a.include_hidden,
        redact_secrets: true,
        format: None,
        ordering: None,
        max_tokens: None,
        include_globs: a.include_globs,
        exclude_globs: a.exclude_globs,
        remove_empty_lines: false,
        truncate_base64: false,
        compress: false,
        strip_comments: false,
        git_diff: false,
        git_log_count: 0,
        emit_skill: false,
        selected_paths: None,
        force_include: Vec::new(),
    };
    let job = match resolve_job(&options) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };
    let scan = match scanner::scan_text_files(&job.root, &job.scan_options) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("scan failed: {}", e);
            return 1;
        }
    };
    let map = crate::repomap::build_repo_map(&job.root, &scan.files, a.tokens);
    match a.out {
        Some(out) if !out.is_empty() => {
            if let Err(e) = std::fs::write(&out, &map) {
                eprintln!("write failed: {}", e);
                return 1;
            }
            println!("out={}", out);
        }
        _ => print!("{}", map),
    }
    0
}

/// Resolve a CLI source: an existing local path passes through; otherwise a
/// remote-looking ref is shallow-cloned (PAT via TURBOMERGER_PAT env, never
/// argv). Returns (root_path, checkout guard to keep alive).
pub(crate) fn resolve_cli_source(
    src: &str,
) -> Result<(String, Option<crate::remote::RemoteCheckout>), String> {
    if std::path::Path::new(src).exists() {
        return Ok((src.to_string(), None));
    }
    if let Some((url, name)) = crate::remote::parse_remote(src) {
        let pat = std::env::var("TURBOMERGER_PAT").ok();
        eprintln!("cloning {} (shallow)...", url);
        let co = crate::remote::clone_shallow(&url, &name, pat.as_deref())?;
        let path = co.path.to_string_lossy().to_string();
        return Ok((path, Some(co)));
    }
    Err(format!("source not found: {}", src))
}

/// Run a headless merge. Returns process exit code.
pub fn run_cli(a: CliArgs) -> i32 {
    if a.src.is_empty() {
        return 2;
    }
    let (src_root, _checkout) = match resolve_cli_source(&a.src) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };
    let options = MergeOptions {
        folder_path: src_root,
        output_path: a.out,
        include_venv: a.include_venv,
        include_tree: true,
        content_detection: true,
        respect_gitignore: !a.no_gitignore,
        include_hidden: a.include_hidden,
        redact_secrets: !a.no_redact,
        format: a.format,
        ordering: a.ordering,
        max_tokens: a.max_tokens,
        include_globs: a.include_globs,
        exclude_globs: a.exclude_globs,
        remove_empty_lines: a.remove_empty_lines,
        truncate_base64: a.truncate_base64,
        compress: a.compress,
        strip_comments: a.strip_comments,
        git_diff: a.git_diff,
        git_log_count: a.git_log,
        emit_skill: a.emit_skill,
        selected_paths: None,
        force_include: Vec::new(),
    };

    let job = match resolve_job(&options) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };

    let scan = match scanner::scan_text_files(&job.root, &job.scan_options) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("scan failed: {}", e);
            return 1;
        }
    };
    if scan.files.is_empty() {
        eprintln!("no text files found");
        return 1;
    }
    let cancel = AtomicBool::new(false);
    let progress = |_c: usize, _t: usize, _f: &str| {};
    match crate::merger::merge_files_with_progress(
        &job.root,
        &scan.files,
        &job.output_path,
        &job.merge_config,
        &cancel,
        progress,
        &scan.skipped,
    ) {
        Ok(o) => {
            if !a.quiet {
                // Aggregate, non-secret output only.
                println!(
                    "merged={} scan_skipped={} merge_skipped={} redacted={} tokens_o200k={} parts={}",
                    o.files_processed,
                    scan.skipped.len(),
                    o.files_skipped,
                    o.secrets_redacted,
                    o.tokens_o200k,
                    o.outputs.len()
                );
                for p in &o.outputs {
                    println!("out={}", p.display());
                }
                if let Some(s) = &o.skill {
                    println!("skill={}", s.display());
                }
            }
            0
        }
        Err(e) => {
            eprintln!("merge failed: {}", e);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_event_filter_ignores_git_and_own_outputs() {
        use std::path::Path;
        assert!(!watch_event_is_relevant(Path::new(
            "C:/repo/.git/index.lock"
        )));
        assert!(!watch_event_is_relevant(Path::new(
            "C:/repo/myrepo_watch_merged.md"
        )));
        assert!(!watch_event_is_relevant(Path::new(
            "C:/repo/myrepo_2026-07-09_merged.part1-of-2.md"
        )));
        assert!(watch_event_is_relevant(Path::new("C:/repo/src/main.rs")));
        assert!(watch_event_is_relevant(Path::new("C:/repo/.gitignore")));
    }

    #[test]
    fn selection_filters_and_force_include_rescues() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(root.join("b.rs"), "fn b() {}\n").unwrap();
        std::fs::write(root.join("notes.txt"), "hello\n").unwrap();

        let mut files = vec![root.join("a.rs"), root.join("b.rs")];
        let mut skipped = vec![crate::scanner::SkipEntry {
            path: "notes.txt".into(),
            reason: "test skip".into(),
        }];

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
