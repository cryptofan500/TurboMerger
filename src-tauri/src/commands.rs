//! Tauri command handlers with progress reporting and cancellation

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use chrono::Local;
use tauri::{AppHandle, Emitter, State};
use std::sync::Mutex;

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
}

#[derive(Debug, Serialize, Clone)]
pub struct ProgressUpdate {
    pub current: usize,
    pub total: usize,
    pub current_file: String,
    pub percentage: f32,
}

#[derive(Debug, Deserialize)]
pub struct MergeOptions {
    pub folder_path: String,
    pub output_path: Option<String>,
    pub include_venv: bool,
    pub include_tree: bool,
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

    // Generate output path
    let folder_name = root.file_name()
        .and_then(|n| n.to_str())
        .map(security::sanitize_filename)
        .unwrap_or_else(|| "merged".to_string());

    let timestamp = Local::now().format("%Y%m%d_%H%M%S");
    let output_name = format!("{}_{}_merged.md", folder_name, timestamp);

    let output_path = match options.output_path {
        Some(ref p) if !p.is_empty() => PathBuf::from(p),
        _ => dirs::download_dir()
            .unwrap_or_else(|| root.parent().unwrap_or(&root).to_path_buf())
            .join(&output_name),
    };

    // Get cancel flag
    let cancel_flag = {
        let state = state.lock().map_err(|e| e.to_string())?;
        state.cancel_flag.clone()
    };

    // Reset cancel flag
    cancel_flag.store(false, Ordering::Relaxed);

    // Emit scanning status
    let _ = app.emit("merge-progress", ProgressUpdate {
        current: 0,
        total: 0,
        current_file: "Scanning directory...".to_string(),
        percentage: 0.0,
    });

    // Scan for text files
    let files = scanner::scan_text_files(&root, options.include_venv)
        .map_err(|e| format!("Scan failed: {}", e))?;

    let file_count = files.len();

    if file_count == 0 {
        return Err("No text files found in directory".to_string());
    }

    // Skip tree if >50k files
    let include_tree = options.include_tree && file_count < 50_000;

    // Merge all files with progress
    let mut files_processed = 0usize;
    let mut files_skipped = 0usize;

    // Run merge
    let total_bytes = crate::merger::merge_files_with_progress(
        &root,
        &files,
        &output_path,
        include_tree,
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
        &mut files_processed,
        &mut files_skipped,
    ).map_err(|e| format!("Merge failed: {}", e))?;

    // Check if cancelled
    if cancel_flag.load(Ordering::Relaxed) {
        // Clean up partial output
        let _ = std::fs::remove_file(&output_path);
        return Err("Operation cancelled by user".to_string());
    }

    let duration_ms = start.elapsed().as_millis() as u64;

    Ok(MergeResult {
        output_path: output_path.to_string_lossy().to_string(),
        files_processed,
        files_skipped,
        total_bytes,
        duration_ms,
    })
}

/// Open a file in the default application
#[tauri::command]
pub fn open_file(path: String) -> Result<(), String> {
    open::that(&path).map_err(|e| format!("Failed to open file: {}", e))
}

/// Open a folder in the file explorer
#[tauri::command]
pub fn open_folder(path: String) -> Result<(), String> {
    let path = PathBuf::from(&path);
    let folder = path.parent().unwrap_or(&path);
    open::that(folder).map_err(|e| format!("Failed to open folder: {}", e))
}
