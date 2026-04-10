mod commands;
mod config;
mod merger;
mod scanner;
pub mod security;

use std::sync::Mutex;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .manage(Mutex::new(commands::AppState::default()))
        .invoke_handler(tauri::generate_handler![
            commands::merge_folder,
            commands::cancel_merge,
            commands::reset_cancel,
            commands::get_downloads_path,
            commands::open_file,
            commands::open_folder,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
