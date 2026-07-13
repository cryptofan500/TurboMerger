# TurboMerger

[![CI](https://github.com/cryptofan500/TurboMerger/actions/workflows/ci.yml/badge.svg)](https://github.com/cryptofan500/TurboMerger/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Platforms](https://img.shields.io/badge/platforms-macOS%20Apple%20Silicon%20%7C%20Windows-lightgrey.svg)

TurboMerger turns a codebase into one structured, LLM-ready file. It is a
local Rust + Tauri 2 desktop app with a React/TypeScript interface, gitignore-aware
scanning, secret redaction, exact token counts, repository maps, and reviewed
apply-back of an LLM response.

The macOS build runs natively on Apple Silicon (`arm64`, including M1–M4) using
WKWebView. The Windows build uses WebView2.

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
src-tauri/target/aarch64-apple-darwin/release/bundle/macos/TurboMerger.app
src-tauri/target/aarch64-apple-darwin/release/bundle/dmg/*.dmg
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

## What it produces

Select a local folder or enter a public repository URL. TurboMerger writes a
timestamped Markdown, Claude XML, XML, JSON, or plain-text snapshot containing:

- a directory tree and linked contents;
- every included text file in collision-safe fences;
- exact `o200k_base` token counts and context-window hints;
- optional token-budget splitting and signature-only compression;
- optional Git diff and recent-commit context; and
- a merge report explaining every included, skipped, unreadable, or redacted file.

## Highlights

- **Gitignore-aware scanning** — honors `.gitignore`, `.ignore`,
  `.git/info/exclude`, and the higher-priority `.turbomergerignore`.
- **Secret safeguards** — excludes credential/key material, detects
  credential-dense documents, and redacts known token formats and labeled values.
- **Curate before merge** — tri-state file tree, token treemap, saved per-project
  selections, and explicit rescue of skipped files.
- **Remote repository packing** — shallow-clones GitHub/GitLab URLs or
  `owner/repo` shorthand into a self-cleaning temporary directory.
- **Compression and repo maps** — tree-sitter signatures plus a ranked,
  budget-aware Aider-style repository map.
- **Watch mode** — debounced regeneration while ignoring Git state, Finder
  metadata, TurboMerger backups, and TurboMerger's own output.
- **Apply-back** — paste a fenced-file response, cxml response, or unified diff;
  preview per-file changes, accept only what you want, create backups, and restore.
- **CLI and MCP** — headless merge/map/apply commands and an MCP stdio server.

## Use the desktop app

1. Select a source folder, or enter a remote repository URL.
2. Keep **Redact secrets** and **Respect .gitignore** enabled for the safest default.
3. Choose a format, ordering, and optional token limit.
4. Optionally use **Scan & curate** before merging.
5. Merge, then open the output or reveal it in Finder/File Explorer.
6. To apply an LLM response, open the **Apply** panel, preview, review, and accept.

## CLI

```text
turbomerger merge <src|owner/repo|URL> [out]
    [--format md|xml|cxml|json|plain] [--ordering path|entry-first|important-last]
    [--max-tokens N] [--include GLOB] [--exclude GLOB]
    [--compress] [--strip-comments] [--git-diff] [--git-log N] [--emit-skill]
    [--no-redact] [--no-gitignore] [--include-hidden] [--include-venv] [--quiet]
turbomerger map <src|owner/repo|URL> [out] [--tokens N]
turbomerger mcp
turbomerger apply <root> --from reply.md [--yes]
turbomerger apply <root> --restore
```

After a source build on Apple Silicon, the binary is at
`src-tauri/target/aarch64-apple-darwin/release/turbomerger`. Private remote
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

- The walker does not follow symlinks/junctions, and broad operating-system roots
  are rejected. Normal macOS projects under `/Users`, external volumes, and safe
  temporary descendants remain usable.
- Sensitive files and credential-dense data files are never merged. Selected
  credential documents may be read harvest-only so their values can be redacted
  if echoed elsewhere; their contents are discarded.
- Apply-back is dry-run first, confines paths to the selected root, refuses binary
  targets and deletions, checks for on-disk changes, and creates restorable backups.
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
npm run test:rust      # 94+ Rust unit/integration tests
npm run tauri:dev      # desktop development mode
```

`npm run verify` runs the complete non-GUI check suite. GitHub CI runs that suite
and a no-bundle Tauri build on Windows and an ARM64 macOS runner. Tagged releases
produce a draft release through a single publisher job; see
[docs/RELEASING.md](docs/RELEASING.md).

## Architecture

```text
src/                         React/TypeScript UI
src-tauri/src/commands.rs    Tauri commands, CLI, watch mode
src-tauri/src/scanner/       gitignore-aware classification
src-tauri/src/security/      path policy, sensitive-file rules, redaction
src-tauri/src/merger/        decode/redact/format/report pipeline
src-tauri/src/compress/      tree-sitter signature compression
src-tauri/src/repomap/       definition/reference ranking
src-tauri/src/remote/        shallow remote clones
src-tauri/src/applyback/     preview/apply/backup/restore
src-tauri/src/mcp/           MCP stdio server
src-tauri/tests/             end-to-end Rust fixtures
```

## License

MIT — see [LICENSE](LICENSE).
