// Prevents console window on Windows in release
#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

fn main() {
    // The command line is its own console binary, `turbomerger` (N-29). For
    // compatibility — scripts that still call this binary, and the AppImage,
    // whose arguments land here — a known subcommand or flag runs it
    // in-process; anything else opens the app.
    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if argv.get(1).is_some_and(|a| tm_cli::is_subcommand(a)) {
        if let Some(code) = tm_cli::run(&argv) {
            std::process::exit(code);
        }
    }
    tm_gui::run();
}
