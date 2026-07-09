//! Tauri command handlers with progress reporting and cancellation

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

use chrono::Local;
use tauri::{AppHandle, Emitter, State};

use crate::scanner;
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

#[derive(Debug, Serialize)]
pub struct MergeResult {
    pub output_path: String,
    pub files_processed: usize,
    pub files_skipped: usize,
    pub total_bytes: usize,
    pub duration_ms: u64,
    pub files_by_extension: usize,
    pub files_by_content: usize,
    pub files_skipped_binary: usize,
    pub files_unreadable: usize,
    pub secrets_redacted: usize,
    pub token_estimate: usize,
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

#[derive(Debug, Deserialize)]
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
}

/// Get the default Downloads folder path
#[tauri::command]
pub fn get_downloads_path() -> Result<String, String> {
    dirs::download_dir()
        .map(|p| p.to_string_lossy().to_string())
        .ok_or_else(|| "Could not find Downloads folder".to_string())
}

/// Cancel the current merge operation
#[tauri::command]
pub fn cancel_merge(state: State<'_, Mutex<AppState>>) -> Result<(), String> {
    let state = state.lock().map_err(|e| e.to_string())?;
    state.cancel_flag.store(true, Ordering::Relaxed);
    Ok(())
}

/// Reset cancellation flag
#[tauri::command]
pub fn reset_cancel(state: State<'_, Mutex<AppState>>) -> Result<(), String> {
    let state = state.lock().map_err(|e| e.to_string())?;
    state.cancel_flag.store(false, Ordering::Relaxed);
    Ok(())
}

/// Main merge operation with progress reporting
#[tauri::command]
pub async fn merge_folder(
    app: AppHandle,
    state: State<'_, Mutex<AppState>>,
    options: MergeOptions,
) -> Result<MergeResult, String> {
    let start = std::time::Instant::now();

    // Validate input path
    let root = security::validate_and_canonicalize(&options.folder_path)
        .map_err(|e| format!("Security error: {}", e))?;

    if !root.exists() || !root.is_dir() {
        return Err("Invalid folder path".to_string());
    }

    // Output naming happens in ONE place, at merge time. If the caller passes a
    // directory (or nothing), a timestamped name is generated inside it.
    let folder_name = root
        .file_name()
        .and_then(|n| n.to_str())
        .map(security::sanitize_filename)
        .unwrap_or_else(|| "merged".to_string());
    let timestamp = Local::now().format("%Y-%m-%dT%H-%M-%S");
    let output_name = format!("{}_{}_merged.md", folder_name, timestamp);

    let output_path = match options.output_path {
        Some(ref p) if !p.is_empty() => {
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

    // Get + reset cancel flag
    let cancel_flag = {
        let state = state.lock().map_err(|e| e.to_string())?;
        state.cancel_flag.clone()
    };
    cancel_flag.store(false, Ordering::Relaxed);

    let _ = app.emit(
        "merge-progress",
        ProgressUpdate {
            current: 0,
            total: 0,
            current_file: "Scanning directory...".to_string(),
            percentage: 0.0,
        },
    );

    // Load per-project config (turbomerger.toml)
    let config = crate::config::load_from_dir(&root);

    // UI checkboxes win; the config file can additionally force inclusions and
    // supplies extension overrides + the size cap.
    let scan_options = scanner::ScanOptions {
        include_venv: options.include_venv || config.scanning.include_venvs,
        content_sniff: options.content_detection && config.scanning.content_sniff,
        include_hidden: options.include_hidden || config.scanning.include_hidden,
        respect_gitignore: options.respect_gitignore,
        max_file_size: config.scanning.max_file_size_mb * 1024 * 1024,
        extra_text_exts: config.extensions.include,
        extra_skip_exts: config.extensions.exclude,
        extra_binary_exts: config.extensions.binary,
    };

    // Scan for text files
    let scan_result = scanner::scan_text_files(&root, &scan_options)
        .map_err(|e| format!("Scan failed: {}", e))?;

    let scan_stats = scan_result.stats;
    let scan_skips = scan_result.skipped;
    let files = scan_result.files;
    let file_count = files.len();

    if file_count == 0 {
        return Err("No text files found in directory".to_string());
    }

    // Skip tree if >50k files
    let include_tree = options.include_tree && file_count < 50_000;

    let outcome = crate::merger::merge_files_with_progress(
        &root,
        &files,
        &output_path,
        include_tree,
        options.redact_secrets,
        &cancel_flag,
        |current, total, file_name| {
            let update = ProgressUpdate {
                current,
                total,
                current_file: file_name.to_string(),
                percentage: (current as f32 / total as f32) * 100.0,
            };
            let _ = app.emit("merge-progress", update);
        },
        &scan_skips,
    )
    .map_err(|e| format!("Merge failed: {}", e))?;

    // Check if cancelled
    if cancel_flag.load(Ordering::Relaxed) {
        let _ = std::fs::remove_file(&output_path);
        return Err("Operation cancelled by user".to_string());
    }

    let duration_ms = start.elapsed().as_millis() as u64;

    Ok(MergeResult {
        output_path: output_path.to_string_lossy().to_string(),
        files_processed: outcome.files_processed,
        files_skipped: outcome.files_skipped + scan_skips.len(),
        total_bytes: outcome.total_bytes,
        duration_ms,
        files_by_extension: scan_stats.by_extension,
        files_by_content: scan_stats.by_content,
        files_skipped_binary: scan_stats.skipped_binary,
        files_unreadable: scan_stats.unreadable,
        secrets_redacted: outcome.secrets_redacted,
        token_estimate: outcome.token_estimate,
    })
}

/// Open a file in the default application
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
