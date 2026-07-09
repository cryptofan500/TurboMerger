mod commands;
pub mod config;
pub mod merger;
pub mod scanner;
pub mod security;
pub mod tokens;

pub use commands::{run_cli, CliArgs};

use std::sync::Mutex;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(Mutex::new(commands::AppState::default()))
        .invoke_handler(tauri::generate_handler![
            commands::merge_folder,
            commands::scan_folder,
            commands::cancel_merge,
            commands::reset_cancel,
            commands::get_downloads_path,
            commands::open_file,
            commands::open_folder,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
