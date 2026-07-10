// Prevents console window on Windows in release
#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

fn main() {
    // Headless CLI path: `turbomerger merge <src> [out] [--flags]`.
    // Lets the app run without the GUI (scripting, CI, safe testing).
    let argv: Vec<String> = std::env::args().collect();
    if let Some(args) = turbomerger::CliArgs::parse(&argv) {
        // Note: a release (GUI-subsystem) build has no attached console, so the
        // stdout summary is only visible in debug or when redirected; the merge
        // output file(s) and the process exit code are the authoritative result.
        std::process::exit(turbomerger::run_cli(args));
    }
    if let Some(args) = turbomerger::MapArgs::parse(&argv) {
        std::process::exit(turbomerger::run_map_cli(args));
    }
    // Apply-back CLI: `turbomerger apply <root> --from reply.md [--yes]`.
    if let Some(args) = turbomerger::ApplyArgs::parse(&argv) {
        std::process::exit(turbomerger::run_apply_cli(args));
    }
    // MCP sidecar: `turbomerger mcp` serves Model Context Protocol on stdio.
    if argv.get(1).map(|s| s.as_str()) == Some("mcp") {
        std::process::exit(turbomerger::run_mcp());
    }
    turbomerger::run();
}
