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
    #[cfg(target_os = "linux")]
    nvidia_wayland_workaround();
    tm_gui::run();
}

/// WebKitGTK's DMA-BUF renderer shows a blank window on NVIDIA's driver
/// under Wayland (N-42). Turn it off there, unless the user decided.
#[cfg(target_os = "linux")]
fn nvidia_wayland_workaround() {
    let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    let wayland = set("WAYLAND_DISPLAY")
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v.eq_ignore_ascii_case("wayland"));
    let nvidia = std::path::Path::new("/proc/driver/nvidia/version").exists()
        || std::path::Path::new("/sys/module/nvidia").exists();
    if wayland && nvidia && std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        // Single-threaded here: nothing else reads the environment yet.
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
}
