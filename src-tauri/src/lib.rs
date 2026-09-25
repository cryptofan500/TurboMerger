//! TurboMerger desktop app: a Tauri shell over `tm-core`. The console
//! command line is the separate `turbomerger` binary (`apps/tm-cli`).

mod commands;

use std::sync::Mutex;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tm_core::cache::enable(None);
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(commands::Jobs::default())
        .manage(commands::Outputs::default())
        .manage(Mutex::new(commands::WatchState::default()))
        .manage(Mutex::new(commands::ApplyUiState::default()))
        .invoke_handler(tauri::generate_handler![
            commands::merge_folder,
            commands::pack_remote,
            commands::scan_folder,
            commands::repo_map,
            commands::start_watch,
            commands::stop_watch,
            commands::cancel_job,
            commands::get_downloads_path,
            commands::open_file,
            commands::open_folder,
            commands::open_web,
            commands::preview_apply,
            commands::apply_accepted,
            commands::restore_backup,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
