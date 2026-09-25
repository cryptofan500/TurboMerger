# TurboMerger

[![CI](https://github.com/cryptofan500/TurboMerger/actions/workflows/ci.yml/badge.svg)](https://github.com/cryptofan500/TurboMerger/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Platforms](https://img.shields.io/badge/platforms-macOS%20Apple%20Silicon%20%7C%20Windows%20%7C%20Linux-lightgrey.svg)

TurboMerger turns a codebase into one structured, LLM-ready file. It is a
local Rust + Tauri 2 desktop app with a React/TypeScript interface, gitignore-aware
scanning, secret redaction, exact token counts, repository maps, and reviewed
apply-back of an LLM response.

The macOS build runs natively on Apple Silicon (`arm64`, including M1–M4) using
WKWebView. The Windows build uses WebView2. The Linux build uses WebKitGTK.

## Install

### macOS Apple Silicon (M1–M4)

The easiest route is the Apple Silicon `.dmg` attached to the latest
[GitHub Release](https://github.com/cryptofan500/TurboMerger/releases).

1. Download the `.dmg` and `SHA256SUMS.txt` from the same release.
2. In Terminal, run `shasum -a 256 ~/Downloads/TurboMerger*.dmg` and compare it
   with the published checksum.
3. Open the DMG and drag TurboMerger into Applications.
4. Launch TurboMerger.

The friend build is ad-hoc signed but not Apple-notarized. If Gatekeeper blocks a
download you trust, try to open it once, then use **System Settings → Privacy &
Security → Open Anyway**. No blanket quarantine-removal command is required.

If a current DMG is not available yet, build it from source:

```bash
git clone https://github.com/cryptofan500/TurboMerger.git
cd TurboMerger

# One-time prerequisites: Xcode Command Line Tools, Node 22/24, Git, Rust stable
xcode-select --install
# Install Node + Git with your preferred package manager, then install Rust from:
# https://rustup.rs

bash scripts/macos-check.sh
rustup target add aarch64-apple-darwin
npm ci
npm run verify
npm run tauri:build:mac
```

Build outputs:

```text
target/aarch64-apple-darwin/release/bundle/macos/TurboMerger.app
target/aarch64-apple-darwin/release/bundle/dmg/*.dmg
```

See [docs/MACOS.md](docs/MACOS.md) for the full M4 runbook, validation commands,
Gatekeeper details, smoke tests, and troubleshooting.

### Windows 10/11

Download the latest `-setup.exe` from
[Releases](https://github.com/cryptofan500/TurboMerger/releases), or build from
source with Node 22/24, stable Rust (MSVC), and Visual Studio Build Tools with
the **Desktop development with C++** workload:

```powershell
git clone https://github.com/cryptofan500/TurboMerger.git
cd TurboMerger
npm ci
npm run verify
npm run tauri:build:windows
```

Use PowerShell or a Developer prompt; Git Bash can put its unrelated `link.exe`
ahead of the MSVC linker.

### Linux (Debian/Ubuntu/Mint x64)

Download the latest `.deb` from
[Releases](https://github.com/cryptofan500/TurboMerger/releases) and install it
with `sudo apt install ./TurboMerger_*.deb`, or use the portable `.AppImage` on
other distros. Verify `SHA256SUMS.txt` from the same release first.

To build from source:

```bash
git clone https://github.com/cryptofan500/TurboMerger.git
cd TurboMerger

# One-time prerequisites (Debian/Ubuntu/Mint):
sudo apt update
sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget file \
  libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev
# Install Node 22/24 and Rust from https://rustup.rs

bash scripts/linux-check.sh
npm ci
npm run verify
npm run tauri:build:linux
```

Build outputs:

```text
target/release/bundle/deb/*.deb
target/release/bundle/appimage/*.AppImage
```

See [docs/LINUX.md](docs/LINUX.md) for the full runbook, the AppImage/FUSE
note, and troubleshooting.

## What it produces

Select a local folder or enter a public repository URL. TurboMerger writes a
timestamped Markdown, Claude XML, XML, JSON, or plain-text snapshot containing:

- a directory tree and linked contents;
- every included text file in collision-safe fences;
- exact `o200k_base` token counts and context-window hints;
- optional token-budget splitting and signature-only compression;
- optional Git diff (of the merged files only) and recent-commit context; and
- a merge report that accounts for every input: what was **not captured**
  (documents and photos without an extractor yet, files too large, unreadable),
  what was skipped on purpose, which directories were not scanned (with file
  counts), and how many entries each ignore rule hid.

Split outputs carry the full tree and a contents list with part numbers in
part 1, the report once, and every part within the token budget. Headers name
the source folder, never your absolute home path (`--show-source-path` opts in).

## Highlights

- **Gitignore-aware scanning** — honors `.gitignore`, `.ignore`,
  `.git/info/exclude`, and the higher-priority `.turbomergerignore`.
- **Secret safeguards** — excludes credential/key material, detects
  credential-dense documents, and redacts known token formats and labeled values.
- **Curate before merge** — tri-state file tree, token treemap, saved per-project
  selections, and explicit rescue of skipped files.
- **Remote repository packing** — shallow-clones GitHub/GitLab URLs or
  `gh:owner/repo` into a self-cleaning temporary directory (Git LFS skipped,
  5-minute timeout; a private-repo token travels in an HTTP header, never in the
  URL or the clone's config).
- **Compression and repo maps** — tree-sitter signatures plus a ranked,
  budget-aware Aider-style repository map.
- **Watch mode** — debounced regeneration while ignoring Git state, Finder
  metadata, TurboMerger backups, and TurboMerger's own output.
- **Apply-back** — paste a fenced-file response, cxml response, or unified diff;
  preview per-file changes, accept only what you want, create backups, and restore.
  Files inside `.git` are never written; CI pipelines, git-hook managers, editor
  and coding-agent settings, and build manifests need an explicit per-file
  confirmation; symlinks are never followed; files keep their encoding.
- **CLI and MCP** — headless merge/map/apply/explain commands and an MCP stdio
  server confined to the folders you share with it.

## Use the desktop app

1. Select a source folder, or enter a remote repository URL.
2. Keep **Redact secrets** and **Respect .gitignore** enabled for the safest default.
3. Choose a format, ordering, and optional token limit.
4. Optionally use **Scan & curate** before merging.
5. Merge, then open the output or reveal it in Finder/File Explorer.
6. To apply an LLM response, open the **Apply** panel, preview, review, and accept.

## CLI

```text
turbomerger merge <folder|URL|gh:owner/repo> [out]
    [--format markdown|xml|cxml|json|plain] [--ordering path|entry-first|important-last]
    [--max-tokens N] [--include GLOB] [--exclude GLOB] [--config FILE] [--max-file-size MB]
    [--compress] [--strip-comments] [--git-diff] [--git-log [N]] [--emit-skill]
    [--no-redact] [--no-gitignore] [--include-hidden] [--include-venv]
    [--show-source-path] [--fail-on-skip] [--quiet]
turbomerger map <folder|URL|gh:owner/repo> [out] [--tokens N]
turbomerger explain <folder> <path>          # why is this path (not) in the merge?
turbomerger apply <root> --from reply.md [--yes]
    [--allow-control GLOB] [--allow-manifest GLOB] [--allow-exec]
turbomerger apply <root> --restore
turbomerger mcp [--root DIR]... [--output-dir DIR] [--allow-remote]
turbomerger completions bash|zsh|fish|powershell|elvish
turbomerger --help | --version
```

Arguments are validated strictly: a typo or a bad number is a usage error
(exit 2) and nothing is written. A bare `owner/repo` is a local path; write
`gh:owner/repo` to clone from GitHub.

| Exit code | Meaning |
|---|---|
| 0 | complete — everything found was merged or excluded on purpose |
| 1 | error |
| 2 | usage error |
| 3 | completed, but some content was **not captured** (or nothing merged, or any skip with `--fail-on-skip`); `apply`: some proposals were held or refused |

The one-line summary on stdout is
`merged=N scan_skipped=N merge_skipped=N redacted=N tokens_o200k=N parts=N not_captured=N`
followed by one `out=<path>` line per output file.

The command line is its own console binary, `turbomerger`; the desktop app is
`turbomerger-gui` (ADR 0015). Installers ship both (the `.deb` puts both in
`/usr/bin`; on macOS the CLI is `TurboMerger.app/Contents/MacOS/turbomerger`).
`turbomerger` with no arguments opens the desktop app when it is installed and a
display is available, and prints this help otherwise. After a source build the
CLI is at `target/release/turbomerger`. Private remote
repositories can use `TURBOMERGER_PAT`; the desktop PAT field remains in memory
and is never persisted.

## Optional project configuration

Place `turbomerger.toml` in the scanned root:

```toml
[extensions]
include = ["myformat"]
exclude = ["log", "tmp"]
binary = ["dat"]

[scanning]
include_hidden = false
include_venvs = false
max_file_size_mb = 2
content_sniff = true
```

UI values take precedence. Use `.turbomergerignore` for path rules.

## Security model

- The walker does not follow symlinks/junctions (links to files inside the root
  are merged once; every other link is listed), and broad operating-system roots
  are rejected. Normal macOS projects under `/Users`, external volumes, Linux
  `/run/media/…` drives and `/run/user/<uid>/…` mounts remain usable.
- Sensitive files and credential-dense data files are never merged. Selected
  credential documents may be read harvest-only so their values can be redacted
  if echoed elsewhere; their contents are discarded.
- Apply-back is dry-run first and does all I/O through directory handles: a
  symlink anywhere on a path is refused, never followed. It never writes inside
  `.git`, holds control files and build manifests for explicit confirmation,
  refuses executable targets unless `--allow-exec`, keeps each file's encoding
  byte-for-byte, refuses binary targets, deletions and `[REDACTED]` placeholders,
  checks for on-disk changes, and creates backups that only this machine can
  restore from.
- The MCP server packs only folders under its `--root` directories, writes only to
  its own outputs folder, and needs `--allow-remote` for remote repositories.
- Merged file contents are marked as untrusted data, and content that could forge
  an output delimiter is escaped (cxml) or cannot match it (plain).
- The WebView has a strict Content Security Policy and no generic filesystem plugin.

No automatic redactor is perfect. Review generated output before uploading it,
especially when disabling gitignore handling or redaction. See [SECURITY.md](SECURITY.md)
for reporting and safe-use guidance.

## Development

```bash
npm ci
npm run check          # versions, ESLint, both TypeScript configs, frontend build
npm run format:check   # rustfmt check
npm run clippy         # warnings are errors
npm run test:rust      # 150+ Rust unit/integration tests, incl. golden outputs
npm run tauri:dev      # desktop development mode
```

`npm run verify` runs the complete non-GUI check suite. GitHub CI runs that suite
and a no-bundle Tauri build on Windows and an ARM64 macOS runner. Tagged releases
produce a draft release through a single publisher job; see
[docs/RELEASING.md](docs/RELEASING.md).

## Architecture

```text
Cargo.toml                   workspace: one version, shared dependency pins, release profile
crates/tm-core/              the engine, no GUI: job setup, scanner, security (path policy,
                             redaction), merger (decode/redact/format/report), compress,
                             repomap, remote clones, apply-back; tests/ = the audit repros
apps/tm-cli/                 `turbomerger`, the console CLI (clap, exit-code contract);
                             tests/golden.rs pins the output bytes (fixtures/golden/)
apps/tm-mcp/                 MCP stdio server, confined to its roots
src-tauri/                   `turbomerger-gui`, the Tauri shell: commands, watch mode
src/                         React/TypeScript UI
fixtures/                    repro scripts, pinned corpora (payloads local-only)
prototypes/                  reference oracles for the document and photo pipelines
docs/adr/                    architecture decision records
```

## License

MIT — see [LICENSE](LICENSE).
