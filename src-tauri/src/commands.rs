//! Tauri command handlers. The work itself is `tm_core`; this module owns
//! the desktop specifics:
//!
//! - Jobs (N-32): every merge or scan gets a job id from the UI and its own
//!   `CancelToken`; `cancel_job` cancels that job only, and the UI treats the
//!   job's promise settling as the acknowledgement.
//! - Nothing heavy runs on an async worker or the main thread (N-33): the
//!   work goes to `spawn_blocking`, and progress (with ETA and stall
//!   reports) streams back over a `tauri::ipc::Channel`.
//! - Watch mode: one worker thread merges on demand; changes that arrive
//!   during a merge trigger one more merge afterwards instead of being lost.
//! - `open_file` / `open_folder` only open outputs this session wrote, and
//!   `open_web` only the three chat sites (N-28).

use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter, Manager, State};

use tm_core::job::{resolve_job, run_merge, JobError, MergeOptions, MergeResult, Progress, Stage};
use tm_core::progress::{Snapshot, Tracker};
use tm_core::{scanner, security, CancelToken};

/// When a job's ETA may be shown (plan §10.3: early rates mislead).
const ETA_AFTER: Duration = Duration::from_secs(60);
/// No progress for this long is reported as a stall.
const STALL_AFTER: Duration = Duration::from_secs(10);

/// Running jobs, by the id the UI chose.
#[derive(Default)]
pub struct Jobs(Mutex<HashMap<String, CancelToken>>);

impl Jobs {
    fn start(&self, id: &str) -> Result<CancelToken, String> {
        let mut jobs = self.0.lock().map_err(|e| e.to_string())?;
        if jobs.contains_key(id) {
            return Err(format!("job {} is already running", id));
        }
        let token = CancelToken::new();
        jobs.insert(id.to_string(), token.clone());
        Ok(token)
    }
    fn end(&self, id: &str) {
        if let Ok(mut jobs) = self.0.lock() {
            jobs.remove(id);
        }
    }
}

/// Every file a merge wrote in this session — the only paths `open_file`
/// and `open_folder` will hand to the OS (N-28).
#[derive(Default)]
pub struct Outputs(Mutex<HashSet<PathBuf>>);

impl Outputs {
    fn record(&self, result: &MergeResult) {
        if let Ok(mut set) = self.0.lock() {
            for p in result.output_paths.iter().chain(result.skill_path.iter()) {
                let p = PathBuf::from(p);
                set.insert(std::fs::canonicalize(&p).unwrap_or(p));
            }
        }
    }
    fn allows(&self, path: &str) -> bool {
        let p = PathBuf::from(path);
        let p = std::fs::canonicalize(&p).unwrap_or(p);
        self.0.lock().map(|set| set.contains(&p)).unwrap_or(false)
    }
}

/// What the UI shows while a job runs.
#[derive(Debug, Clone, Serialize)]
pub struct JobProgress {
    pub stage: Stage,
    pub done: usize,
    pub total: usize,
    pub current: String,
    pub elapsed_ms: u64,
    pub eta_ms: Option<u64>,
    pub eta_band_ms: Option<u64>,
    pub stalled_ms: Option<u64>,
}

impl From<Snapshot> for JobProgress {
    fn from(s: Snapshot) -> Self {
        let ms = |d: Duration| d.as_millis() as u64;
        JobProgress {
            stage: s.stage,
            done: s.done,
            total: s.total,
            current: s.current,
            elapsed_ms: ms(s.elapsed),
            eta_ms: s.eta.map(ms),
            eta_band_ms: s.eta_band.map(ms),
            stalled_ms: s.stalled_for.map(ms),
        }
    }
}

/// Feeds a job's progress to its channel: every event goes to the tracker;
/// the UI gets at most ten updates a second, plus a tick every second so
/// the ETA and stall reports stay current when nothing arrives.
struct Reporter {
    tracker: Arc<Tracker>,
    channel: Channel<JobProgress>,
    last: Mutex<Instant>,
    stop: Arc<AtomicBool>,
    ticker: Option<std::thread::JoinHandle<()>>,
}

impl Reporter {
    fn start(channel: Channel<JobProgress>) -> Reporter {
        let tracker = Arc::new(Tracker::new(ETA_AFTER, STALL_AFTER));
        let stop = Arc::new(AtomicBool::new(false));
        let ticker = {
            let (tracker, channel, stop) = (tracker.clone(), channel.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut last_tick = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(100));
                    if last_tick.elapsed() >= Duration::from_secs(1) {
                        last_tick = Instant::now();
                        let _ = channel.send(tracker.snapshot().into());
                    }
                }
            })
        };
        Reporter {
            tracker,
            channel,
            last: Mutex::new(Instant::now() - Duration::from_secs(1)),
            stop,
            ticker: Some(ticker),
        }
    }

    fn on(&self, p: Progress) {
        self.tracker.update(&p);
        let mut last = self.last.lock().expect("reporter");
        if last.elapsed() >= Duration::from_millis(100) {
            *last = Instant::now();
            let _ = self.channel.send(self.tracker.snapshot().into());
        }
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.ticker.take() {
            let _ = t.join();
        }
    }
}

/// Run blocking work off the async runtime, as one registered job.
async fn job<T, F>(jobs: &Jobs, id: &str, work: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(CancelToken) -> Result<T, String> + Send + 'static,
{
    let token = jobs.start(id)?;
    let result = tauri::async_runtime::spawn_blocking(move || work(token))
        .await
        .map_err(|e| format!("job failed: {}", e));
    jobs.end(id);
    result?
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
pub async fn scan_folder(
    jobs: State<'_, Jobs>,
    options: MergeOptions,
    job: String,
    progress: Channel<JobProgress>,
) -> Result<ScanReport, String> {
    self::job(&jobs, &job, move |token| {
        let start = Instant::now();
        let resolved = resolve_job(&options)?;
        let reporter = Reporter::start(progress);
        let on_scan = |p: scanner::ScanProgress| {
            reporter.on(match p {
                scanner::ScanProgress::Walked(n) => Progress {
                    stage: Stage::Scan,
                    done: n,
                    total: 0,
                    current: String::new(),
                },
                scanner::ScanProgress::Classified { done, total } => Progress {
                    stage: Stage::Classify,
                    done,
                    total,
                    current: String::new(),
                },
            })
        };
        let scan = scanner::scan_with(
            &resolved.root,
            &resolved.scan_options,
            token.flag(),
            &on_scan,
        )
        .map_err(|e| {
            if e.is::<tm_core::Cancelled>() {
                JobError::Cancelled.to_string()
            } else {
                format!("Scan failed: {}", e)
            }
        })?;

        use rayon::prelude::*;
        let total = scan.files.len();
        let counter = std::sync::atomic::AtomicUsize::new(0);
        let included: Vec<Option<ScanEntry>> = scan
            .files
            .par_iter()
            .map(|path| {
                if token.is_cancelled() {
                    return None;
                }
                let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                let tokens = std::fs::read(path)
                    .map(|bytes| tm_core::tokens::count(&String::from_utf8_lossy(&bytes)))
                    .unwrap_or(0);
                let done = counter.fetch_add(1, Ordering::Relaxed) + 1;
                let rel = scanner::relative_display(&resolved.root, path);
                if done.is_multiple_of(32) || done == total {
                    reporter.on(Progress {
                        stage: Stage::Merge,
                        done,
                        total,
                        current: rel.clone(),
                    });
                }
                Some(ScanEntry {
                    path: rel,
                    size,
                    tokens,
                })
            })
            .collect();
        if token.is_cancelled() {
            return Err(JobError::Cancelled.to_string());
        }
        let mut included: Vec<ScanEntry> = included.into_iter().flatten().collect();
        included.sort_by(|a, b| a.path.cmp(&b.path));
        let total_tokens = included.iter().map(|e| e.tokens).sum();
        Ok(ScanReport {
            root: resolved.root.to_string_lossy().to_string(),
            included,
            skipped: scan.skipped,
            total_tokens,
            duration_ms: start.elapsed().as_millis() as u64,
        })
    })
    .await
}

/// Build an aider-style repo map (tags → PageRank → budgeted signatures).
#[tauri::command]
pub async fn repo_map(
    options: MergeOptions,
    token_budget: Option<usize>,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let job = resolve_job(&options)?;
        let scan = scanner::scan_text_files(&job.root, &job.scan_options)
            .map_err(|e| format!("Scan failed: {}", e))?;
        Ok(tm_core::repomap::build_repo_map(
            &job.root,
            &scan.files,
            token_budget.unwrap_or(1024),
        ))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub fn get_downloads_path() -> Result<String, String> {
    dirs::download_dir()
        .map(|p| p.to_string_lossy().to_string())
        .ok_or_else(|| "Could not find Downloads folder".to_string())
}

/// Cancel one running job. `false` when no such job is running (it may
/// have just finished).
#[tauri::command]
pub fn cancel_job(jobs: State<'_, Jobs>, job: String) -> Result<bool, String> {
    let jobs = jobs.0.lock().map_err(|e| e.to_string())?;
    Ok(match jobs.get(&job) {
        Some(token) => {
            token.cancel();
            true
        }
        None => false,
    })
}

#[tauri::command]
pub async fn merge_folder(
    jobs: State<'_, Jobs>,
    outputs: State<'_, Outputs>,
    options: MergeOptions,
    job: String,
    progress: Channel<JobProgress>,
) -> Result<MergeResult, String> {
    let result = self::job(&jobs, &job, move |token| {
        let reporter = Reporter::start(progress);
        run_merge(&options, None, &token, &|p| reporter.on(p))
            .map(|run| run.result)
            .map_err(|e| e.to_string())
    })
    .await?;
    outputs.record(&result);
    Ok(result)
}

/// Pack a remote repo: shallow-clone (temp, self-cleaning) then run the
/// normal merge pipeline over the checkout. `pat` stays in memory only.
#[tauri::command]
pub async fn pack_remote(
    jobs: State<'_, Jobs>,
    outputs: State<'_, Outputs>,
    url: String,
    pat: Option<String>,
    options: MergeOptions,
    job: String,
    progress: Channel<JobProgress>,
) -> Result<MergeResult, String> {
    let result = self::job(&jobs, &job, move |token| {
        let (clone_url, name) = tm_core::remote::parse_remote(&url)
            .ok_or("Not a recognizable repo reference (URL or owner/repo)")?;
        let reporter = Reporter::start(progress);
        reporter.on(Progress {
            stage: Stage::Scan,
            done: 0,
            total: 0,
            current: format!("Cloning {} (shallow)...", clone_url),
        });
        let checkout = tm_core::remote::clone_shallow(&clone_url, &name, pat.as_deref())?;
        if token.is_cancelled() {
            return Err(JobError::Cancelled.to_string());
        }
        let mut opts = options;
        opts.folder_path = checkout.path.to_string_lossy().to_string();
        opts.source_label = Some(clone_url);
        opts.remote = true;
        run_merge(&opts, None, &token, &|p| reporter.on(p))
            .map(|run| run.result)
            .map_err(|e| e.to_string())
        // checkout drops here: temp clone deleted.
    })
    .await?;
    outputs.record(&result);
    Ok(result)
}

// ============================================================================
// WATCH MODE (T2-6)
// ============================================================================

/// The live watch: dropping it stops the watcher, ends the worker thread and
/// cancels a merge in flight.
#[derive(Default)]
pub struct WatchState {
    active: Option<ActiveWatch>,
}

struct ActiveWatch {
    _debouncer: notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>,
    cancel: CancelToken,
}

impl Drop for ActiveWatch {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Filesystem events that must NOT retrigger a watch merge: VCS/app state,
/// Finder metadata, and our own outputs.
fn watch_event_is_relevant(path: &Path) -> bool {
    if path
        .components()
        .any(|c| matches!(c.as_os_str().to_str(), Some(".git" | ".turbomerger")))
    {
        return false;
    }
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        if name.eq_ignore_ascii_case(".DS_Store") || name.to_ascii_lowercase().contains("_merged.")
        {
            return false;
        }
    }
    true
}

fn emit_watch_result(app: &AppHandle, result: &Result<MergeResult, String>) {
    match result {
        Ok(r) => {
            app.state::<Outputs>().record(r);
            let _ = app.emit("watch-merged", r.clone());
        }
        Err(e) => {
            let _ = app.emit("watch-error", e.clone());
        }
    }
}

/// Start watching `options.folder_path`; re-merge (debounced 300 ms) on
/// changes into one stable output. Returns that output's path.
#[tauri::command]
pub async fn start_watch(app: AppHandle, options: MergeOptions) -> Result<String, String> {
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
    let cancel = CancelToken::new();

    // Merge once up front so the output exists before the first change.
    let first = {
        let (options, output, cancel) = (options.clone(), output.clone(), cancel.clone());
        tauri::async_runtime::spawn_blocking(move || {
            run_merge(&options, Some(&output), &cancel, &|_| {})
                .map(|r| r.result)
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())?
    };
    emit_watch_result(&app, &first);
    first?;

    // Changes become signals; one worker merges per burst, and a change that
    // lands during a merge makes it run again (nothing is dropped).
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    {
        let (app, options, output, cancel) =
            (app.clone(), options.clone(), output.clone(), cancel.clone());
        std::thread::spawn(move || {
            while rx.recv().is_ok() {
                while rx.try_recv().is_ok() {}
                if cancel.is_cancelled() {
                    break;
                }
                let result = run_merge(&options, Some(&output), &cancel, &|_| {})
                    .map(|r| r.result)
                    .map_err(|e| e.to_string());
                if cancel.is_cancelled() {
                    break;
                }
                emit_watch_result(&app, &result);
            }
        });
    }
    let app2 = app.clone();
    let mut debouncer = notify_debouncer_mini::new_debouncer(
        Duration::from_millis(300),
        move |res: notify_debouncer_mini::DebounceEventResult| match res {
            Ok(events) => {
                if events.iter().any(|e| watch_event_is_relevant(&e.path)) {
                    let _ = tx.send(());
                }
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

    let state = app.state::<Mutex<WatchState>>();
    state.lock().map_err(|e| e.to_string())?.active = Some(ActiveWatch {
        _debouncer: debouncer,
        cancel,
    });
    Ok(output.to_string_lossy().to_string())
}

#[tauri::command]
pub fn stop_watch(watch: State<'_, Mutex<WatchState>>) -> Result<(), String> {
    // Dropping the watch stops the watcher and its worker.
    watch.lock().map_err(|e| e.to_string())?.active.take();
    Ok(())
}

// ============================================================================
// APPLY-BACK (T3-3)
// ============================================================================

/// The last parsed preview, held server-side so apply never round-trips the
/// proposed file contents through the webview.
#[derive(Default)]
pub struct ApplyUiState {
    pending: Option<PendingApply>,
}

struct PendingApply {
    root: PathBuf,
    ready: Vec<tm_core::applyback::ReadyFile>,
}

/// Parse a pasted LLM reply against `root` and return per-file diffs.
/// Dry-run: nothing is written; the appliable set is parked in state.
#[tauri::command]
pub async fn preview_apply(
    app: AppHandle,
    root: String,
    reply: String,
) -> Result<tm_core::applyback::Preview, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let root = security::validate_and_canonicalize(&root)
            .map_err(|e| format!("Security error: {}", e))?;
        if !root.is_dir() {
            return Err("Target root is not a folder".to_string());
        }
        let changes = tm_core::applyback::parse_reply(&reply);
        if changes.is_empty() {
            return Err(
                "No file changes recognized. Supported: `## path` + fenced code block, \
                 cxml <source> documents, and unified diffs (--- / +++ / @@)."
                    .to_string(),
            );
        }
        let built = tm_core::applyback::build_preview(
            &root,
            &changes,
            &tm_core::applyback::ApplyPolicy::default(),
        )?;
        let state = app.state::<Mutex<ApplyUiState>>();
        state.lock().map_err(|e| e.to_string())?.pending = Some(PendingApply {
            root,
            ready: built.ready,
        });
        Ok(built.preview)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Write the accepted subset of the last preview (with backups). One-shot:
/// the pending preview is consumed; re-parse for another round. `confirm`
/// lists control/manifest files the user confirmed one by one in the UI —
/// without it they are refused like on the CLI.
#[tauri::command]
pub async fn apply_accepted(
    app: AppHandle,
    root: String,
    accept: Vec<String>,
    confirm: Option<Vec<String>>,
) -> Result<tm_core::applyback::ApplyOutcome, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let pending = app
            .state::<Mutex<ApplyUiState>>()
            .lock()
            .map_err(|e| e.to_string())?
            .pending
            .take()
            .ok_or("Nothing parsed — paste a reply and preview it first")?;
        let root = security::validate_and_canonicalize(&root)
            .map_err(|e| format!("Security error: {}", e))?;
        if root != pending.root {
            return Err("Preview is for a different folder — re-parse the reply".to_string());
        }
        let want: HashSet<&str> = accept.iter().map(|s| s.as_str()).collect();
        let files: Vec<tm_core::applyback::ReadyFile> = pending
            .ready
            .into_iter()
            .filter(|f| want.contains(f.rel_path.as_str()))
            .collect();
        if files.is_empty() {
            return Err("No accepted files to apply".to_string());
        }
        let mut policy = tm_core::applyback::ApplyPolicy::default();
        policy.confirmed = confirm
            .unwrap_or_default()
            .into_iter()
            .filter(|c| want.contains(c.as_str()))
            .collect();
        tm_core::applyback::apply_files(&root, &files, &policy)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Reverse the most recent apply for `root` from its backup manifest.
#[tauri::command]
pub async fn restore_backup(root: String) -> Result<tm_core::applyback::RestoreOutcome, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let root = security::validate_and_canonicalize(&root)
            .map_err(|e| format!("Security error: {}", e))?;
        tm_core::applyback::restore_last(&root, &tm_core::applyback::ApplyPolicy::default())
    })
    .await
    .map_err(|e| e.to_string())?
}

// ============================================================================
// OPENING THINGS (N-28)
// ============================================================================

/// Open an output of this session with its default application.
#[tauri::command]
pub fn open_file(outputs: State<'_, Outputs>, path: String) -> Result<(), String> {
    if !outputs.allows(&path) {
        return Err("Only files this session wrote can be opened".to_string());
    }
    open::that(&path).map_err(|e| format!("Failed to open file: {}", e))
}

/// Open one of the chat sites the result panel links to.
#[tauri::command]
pub fn open_web(site: String) -> Result<(), String> {
    let url = match site.as_str() {
        "claude" => "https://claude.ai/new",
        "chatgpt" => "https://chatgpt.com",
        "gemini" => "https://gemini.google.com/app",
        _ => return Err(format!("Unknown site: {}", site)),
    };
    open::that(url).map_err(|e| format!("Failed to open {}: {}", url, e))
}

/// Reveal an output of this session in the file manager, selected.
#[tauri::command]
pub fn open_folder(outputs: State<'_, Outputs>, path: String) -> Result<(), String> {
    if !outputs.allows(&path) {
        return Err("Only files this session wrote can be shown".to_string());
    }
    reveal(Path::new(&path))
}

#[cfg(windows)]
fn reveal(path: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    // One argument, quoted the way Explorer parses it: `/select,"C:\a b\c.md"`.
    // std's own quoting would wrap all of `/select,…` in quotes, which
    // Explorer does not understand. Paths cannot contain `"` on Windows.
    let path = path.to_string_lossy();
    let path = path.strip_prefix(r"\\?\").unwrap_or(&path);
    std::process::Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{}\"", path))
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Failed to open folder: {}", e))
}

#[cfg(target_os = "macos")]
fn reveal(path: &Path) -> Result<(), String> {
    std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Failed to open folder in Finder: {}", e))
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn reveal(path: &Path) -> Result<(), String> {
    // The freedesktop file-manager interface selects the file (Nautilus,
    // Nemo, Dolphin, Thunar…); without it, open the parent folder.
    let shown = std::process::Command::new("dbus-send")
        .args([
            "--session",
            "--print-reply",
            "--dest=org.freedesktop.FileManager1",
            "--type=method_call",
            "/org/freedesktop/FileManager1",
            "org.freedesktop.FileManager1.ShowItems",
        ])
        .arg(format!("array:string:{}", file_uri(path)))
        .arg("string:")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if shown {
        return Ok(());
    }
    let folder = path.parent().unwrap_or(path);
    open::that(folder).map_err(|e| format!("Failed to open folder: {}", e))
}

/// `file://` URI with percent-encoding for everything outside the
/// unreserved set and `/` (also `,`, which dbus-send splits arrays on).
#[cfg(all(not(windows), not(target_os = "macos")))]
fn file_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_event_filter_ignores_git_and_own_outputs() {
        assert!(!watch_event_is_relevant(Path::new(
            "C:/repo/.git/index.lock"
        )));
        assert!(!watch_event_is_relevant(Path::new(
            "C:/repo/myrepo_watch_merged.md"
        )));
        assert!(!watch_event_is_relevant(Path::new(
            "C:/repo/myrepo_2026-07-09_merged.part1-of-2.md"
        )));
        assert!(!watch_event_is_relevant(Path::new(
            "/Users/me/repo/.turbomerger/backups/manifest.json"
        )));
        assert!(!watch_event_is_relevant(Path::new(
            "/Users/me/repo/.DS_Store"
        )));
        assert!(watch_event_is_relevant(Path::new("C:/repo/src/main.rs")));
        assert!(watch_event_is_relevant(Path::new("C:/repo/.gitignore")));
    }

    #[test]
    fn only_session_outputs_may_be_opened() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("r_merged.md");
        std::fs::write(&out, "x").unwrap();
        let outputs = Outputs::default();
        assert!(!outputs.allows(&out.to_string_lossy()));
        outputs.record(&MergeResult {
            output_path: out.to_string_lossy().to_string(),
            output_paths: vec![out.to_string_lossy().to_string()],
            files_processed: 1,
            files_skipped: 0,
            total_bytes: 1,
            duration_ms: 0,
            files_by_extension: 1,
            files_by_content: 0,
            files_skipped_binary: 0,
            files_unreadable: 0,
            secrets_redacted: 0,
            tokens_o200k: 1,
            tokens_claude_est: 1,
            skill_path: None,
            not_captured: 0,
        });
        assert!(outputs.allows(&out.to_string_lossy()));
        assert!(!outputs.allows("/etc/passwd"));
        assert!(!outputs.allows(&tmp.path().to_string_lossy()));
    }

    #[cfg(all(not(windows), not(target_os = "macos")))]
    #[test]
    fn file_uris_are_percent_encoded() {
        assert_eq!(
            file_uri(Path::new("/home/a b/ü#1,2.md")),
            "file:///home/a%20b/%C3%BC%231%2C2.md"
        );
    }
}
