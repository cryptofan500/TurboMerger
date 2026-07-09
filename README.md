# TurboMerger

A fast Windows desktop app that merges a codebase into a single, LLM-ready markdown
file — gitignore-aware, secret-redacting, with a per-file merge report. Built with
Rust + Tauri 2 and a React/TypeScript UI (WebView2).

![License](https://img.shields.io/badge/license-MIT-blue.svg)
![Platform](https://img.shields.io/badge/platform-Windows-lightgrey.svg)

## What it does

Point it at a folder; it produces `<folder>_<timestamp>_merged.md` in your Downloads
(or a location you pick) containing a directory tree, a linked table of contents, every
text file wrapped in a collision-proof code fence, and a **Merge Report** listing exactly
what was skipped and why. Designed for pasting a whole project into ChatGPT / Claude /
Gemini.

## Features

- **Gitignore-aware scanning** — honors `.gitignore`, `.ignore`, `.git/info/exclude`, and
  a highest-precedence `.turbomergerignore` (via the `ignore` crate, ripgrep's walker).
  Works even outside a git repo. Toggle off if you want everything.
- **Real token counting** — exact `o200k_base` (tiktoken) counts per file and per merge, a
  Claude estimate, and a fit hint against GPT 128k / Claude 200k / Gemini 1M.
- **Output formats** — Markdown, **Claude XML (cxml)**, XML, JSON, or Plain text.
- **Token-budget splitting** — cap tokens and the output splits at file boundaries into
  `…part1-of-N` files, each labelled "Part N/M".
- **Ordering** — alphabetical, entry-points-first, or important-last (README/entry files at
  the end, where LLMs weight context most).
- **Include/exclude globs** — UI + `turbomerger.toml [filter]`; plus content slimming
  (remove empty lines, truncate long base64 blobs). **Presets**: LLM-review-lean,
  Claude-cxml, Full archive, Docs only.
- **Secret redaction** — API keys, tokens, private keys, and DB connection strings
  (AWS, GitHub, GitLab, OpenAI, Anthropic, Google, Stripe, Slack, JWTs, …) are replaced
  with `[REDACTED]` before writing (entropy gate + placeholder stopwords). Credential
  *data files* and *credential-dense* files are excluded wholesale (see *Security*).
  On by default; counted in the report.
- **Nothing dropped silently** — every excluded file appears in the Merge Report with a
  reason (binary, too large, gitignored, sensitive, credential-dense, unreadable, …).
- **Headless CLI** — `turbomerger merge <src> [out] [--flags]` for scripting/CI; drag a
  folder onto the window; settings persist; open-in-chat links.
- **Sensible dotfile handling** — well-known config dotfiles (`.gitignore`, `.mcp.json`,
  `.github/`, `.env.example`, `.eslintrc*`, …) are included; sensitive ones (`.env`,
  `*.pem`, `id_rsa`) are excluded-with-reason. "Include hidden dotfiles" opts in to the rest.
- **Content-based text detection** — files with unknown extensions are classified by
  sniffing the first 8 KB (magic bytes, null/control/non-ASCII ratios, line length).
- **Collision-proof fences** — a file containing ```` ``` ```` gets a longer wrapper fence,
  so markdown-heavy repos don't scramble the output.
- **Token estimate + fit hint** — reports approximate tokens and whether the result fits
  common context windows (GPT 128k / Claude 200k / Gemini 1M).
- **Parallel + memory-bounded** — parallel directory walk (ripgrep walker) and parallel,
  chunked file processing (Rayon); output is streamed in batches rather than held whole.
- **Safe by construction** — symlinks/junctions never followed; system roots blocked;
  cloud-placeholder files skipped rather than force-downloaded.

## Install

Grab the latest `.msi` or `-setup.exe` from
[Releases](https://github.com/cryptofan500/TurboMerger/releases), or build from source.

### Build from source

Requirements: Windows 10/11, Node.js 20+, Rust (stable, MSVC target), Visual Studio Build
Tools with the "Desktop development with C++" workload.

```powershell
git clone https://github.com/cryptofan500/TurboMerger.git
cd TurboMerger
npm install
npm run tauri:build   # exe + NSIS/MSI installers under src-tauri/target/release/
```

> Build from a PowerShell / Developer prompt, **not** Git Bash — Git Bash's `link.exe`
> shadows the MSVC linker and breaks the build.

## Usage

1. Launch TurboMerger.
2. **Browse** to a source folder.
3. Adjust options (respect gitignore, redact secrets, tree/TOC, content detection, hidden
   files, virtual environments).
4. Optionally **Change** the output path (a folder auto-names the file; a file path is used
   verbatim).
5. **Merge**, then **Open File** or **Show in Folder**.

## Configuration — `turbomerger.toml` (optional)

Drop a `turbomerger.toml` in the scanned folder's root to override behavior per project:

```toml
[extensions]
include = ["myformat", "custom1"]  # extra extensions to treat as text
exclude = ["log", "tmp"]           # extensions to always skip
binary  = ["dat"]                  # extra extensions to treat as binary

[scanning]
include_hidden   = false
include_venvs    = false
max_file_size_mb = 2               # absolute per-file cap
content_sniff    = true
```

UI checkboxes take precedence; the config file can additionally force inclusions and
supplies the extension overrides + size cap. For path-level control, add a
`.turbomergerignore` (gitignore syntax, highest precedence).

## How detection works

Each candidate file runs through: config exclude list → known binary extension → known text
extension → content sniff (magic bytes → null-byte ratio → control-char ratio → non-ASCII
ratio → max line length). Directories in the always-skip set (`.git`, `node_modules`,
`target`, build/cache dirs, credential dirs) and gitignored paths are pruned before descent.
Lock files, minified bundles, and previous TurboMerger outputs are skipped by name.

## Security

- Symlinks and reparse points (junctions) are never followed.
- Windows system roots (`C:\Windows`, `Program Files`, `ProgramData`, …) are blocked as
  scan roots.
- Sensitive files (`.env`, key/cert material, SSH keys, credential stores) are excluded and
  listed in the report.
- **Credential data files** (e.g. `*_CREDENTIALS_*.md`, `passwords.csv`, `vault.txt`,
  `*.secrets.yaml`) and **credential-dense files** (content with multiple inline logins,
  Google app-passwords, or key blocks) are excluded wholesale — never partially redacted.
  Source files that merely mention the words (`password_reset.py`, `useApiKey.ts`) are not
  affected.
- Detected secrets are redacted from file contents before writing, including context-gated
  Google app-passwords and `email:password` values.
- **Limitation:** no redactor catches every credential embedded in free-form prose. Default
  (gitignore-respecting) mode is safest — it prunes ignored credential docs entirely. The
  **Full archive / `--no-gitignore`** mode deliberately includes everything and may surface
  prose-embedded secrets; review the output before uploading.
- Strict Content-Security-Policy on the WebView.

## Development

```powershell
npm install
npm run tauri:dev     # hot-reload GUI
npm run typecheck     # tsc --noEmit
npm run lint          # eslint
cargo test --manifest-path src-tauri/Cargo.toml   # Rust unit + integration tests
```

## Architecture

```
src/                     React/TypeScript UI (App.tsx + styles)
src-tauri/src/
  lib.rs                 Tauri entry point + command registration
  commands.rs            IPC handlers (merge_folder, cancel, open, …)
  config.rs              turbomerger.toml loader
  scanner/mod.rs         gitignore-aware walk + text/binary classification
  security/mod.rs        path validation, binary detection, secret redaction
  merger/mod.rs          parallel read/redact + markdown writer + merge report
src-tauri/tests/         end-to-end fixture-tree tests
```

## License

MIT — see [LICENSE](LICENSE).
