pub mod applyback;
mod commands;
pub mod compress;
pub mod config;
pub mod mcp;
pub mod merger;
pub mod remote;
pub mod repomap;
pub mod scanner;
pub mod security;
pub mod tokens;

pub use commands::{run_apply_cli, run_cli, run_map_cli, ApplyArgs, CliArgs, MapArgs};
pub use mcp::run_mcp;

use std::sync::Mutex;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(Mutex::new(commands::AppState::default()))
        .manage(Mutex::new(commands::WatchState::default()))
        .manage(Mutex::new(commands::ApplyUiState::default()))
        .invoke_handler(tauri::generate_handler![
            commands::merge_folder,
            commands::pack_remote,
            commands::scan_folder,
            commands::repo_map,
            commands::start_watch,
            commands::stop_watch,
            commands::cancel_merge,
            commands::reset_cancel,
            commands::get_downloads_path,
            commands::open_file,
            commands::open_folder,
            commands::preview_apply,
            commands::apply_accepted,
            commands::restore_backup,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
