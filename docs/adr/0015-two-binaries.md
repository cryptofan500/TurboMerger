# 0015 — Two binaries: `turbomerger` (console CLI) and `turbomerger-gui`

- **Status:** Accepted (2026-09-24) — implemented (Phase 2.1–2.4; finding N-29)
- **Context:**
  - v7 shipped one binary for the app and the command line. On Windows the
    release build is a GUI-subsystem executable, so the CLI printed nothing to
    a terminal and a shell could not wait for its exit code (N-29).
  - A static-musl CLI (ADR 0002) cannot link WebKitGTK, so the CLI must not
    depend on Tauri at all.
  - The plan proposed naming the app `TurboMerger`. That collides with
    `turbomerger` on case-insensitive file systems (APFS by default on macOS,
    NTFS on Windows), and installers put the CLI next to the app.
- **Decision:**
  - The workspace has `crates/tm-core` (the engine, no GUI), `apps/tm-cli`,
    `apps/tm-mcp` and `src-tauri` (package `tm-gui`).
  - `turbomerger` is the CLI on every OS: a console binary from `tm-cli`, with
    no Tauri or GTK code. It ships in the GUI installers as a Tauri sidecar
    (`bundle.externalBin`, staged by `scripts/build-sidecar.mjs`). The
    standalone archives (musl included) contain it alone.
  - `turbomerger-gui` is the desktop app on every OS. The installers still
    call it "TurboMerger" through `productName`. The Linux `.desktop` entry
    runs `turbomerger-gui`, and the `.deb` installs both binaries in
    `/usr/bin`, so `turbomerger merge …` keeps working after an upgrade.
  - `turbomerger` with no arguments opens `turbomerger-gui` when it sits
    next to the CLI (or is on `PATH`) and a display is available. Otherwise
    it prints help and exits 2.
  - `turbomerger-gui` given a CLI subcommand or flag runs the CLI in-process.
    This keeps old scripts and the AppImage (whose arguments reach the app
    binary) working. Any other argument opens the app.
  - The sidecar lives only in `src-tauri/tauri.bundle.conf.json`. Plain
    `cargo build`/`clippy`/`tauri dev` never need a staged sidecar, and
    `tauri build --config src-tauri/tauri.bundle.conf.json` stages it through
    its `beforeBuildCommand`.
- **Consequences:**
  - Build output moved from `src-tauri/target` to `./target` (workspace
    root). The release workflow, docs and version check read the new layout.
    The version lives once, in `[workspace.package]`.
  - Icons in the `.deb` are named `turbomerger-gui`.
  - Windows: `turbomerger.exe` in the install folder is the console CLI. It is
    not on `PATH` unless the user adds it; a standalone CLI package comes in
    Phase 8.
