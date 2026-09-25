// Prevents console window on Windows in release
#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

fn main() {
    // Any argument means the headless CLI (`merge`, `map`, `apply`, `explain`,
    // `mcp`, `completions`, `--help`, `--version`); parsing is strict and never
    // falls through to the GUI, which aborts without a display (N-30).
    // Note: a release (GUI-subsystem) Windows build has no attached console, so
    // stdout is only visible when redirected; the output files and the exit
    // code are the authoritative result (two binaries arrive in v8, N-29).
    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if let Some(code) = turbomerger::cli::run(&argv) {
        std::process::exit(code);
    }
    turbomerger::run();
}
