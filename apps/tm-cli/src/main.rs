//! `turbomerger`: the console command line. A console binary on every OS
//! (N-29: on Windows the desktop app has no console, so its output and exit
//! codes never reached a terminal). The desktop app is `turbomerger-gui`.

// musl's allocator serializes the rayon workers; mimalloc keeps the static
// Linux build as fast as the glibc one (and helps the others).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let code = match tm_cli::run(&argv) {
        Some(code) => code,
        None => tm_cli::launch_gui_or_help(),
    };
    std::process::exit(code);
}
