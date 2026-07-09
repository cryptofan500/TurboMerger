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
- **Compress to signatures** — tree-sitter elides function bodies (`{ ... }` / `...`)
  across rs/js/jsx/ts/tsx/py/go/java/c/cpp, keeping signatures, types, imports, and class
  structure (~60–80% token cut); separate strip-comments toggle. Unparseable files pass
  through unchanged.
- **Repo map** — `turbomerger map <src>` builds an aider-style ranked signature map
  (tree-sitter def/ref tags → PageRank → token budget) for repos that won't fit whole.
- **Curate before merging** — **Scan & curate** opens a tri-state checkbox file tree with
  per-file token counts and a selected-vs-budget bar, a click-to-exclude token **treemap**,
  and a skip-report drill-in with per-file "include anyway" rescue. Selection persists per
  project.
- **Watch mode** — re-merges (debounced) into a stable `<repo>_watch_merged.*` file on
  every change; `.git` churn and own outputs ignored.
- **Git context** — optionally append the working-tree diff (`--git-diff`) and recent
  commits (`--git-log N`) as final, redacted sections.
- **Remote repos** — paste `owner/repo` or a GitHub/GitLab URL: shallow clone to a
  self-cleaning temp dir → normal pipeline; PAT held in memory only.
- **MCP server** — `turbomerger mcp` serves `pack_directory` / `repo_map` / `read_output` /
  `grep_output` to Claude Desktop/Code over stdio (redaction forced on).
- **Claude skill** — optional `.claude/skills/<repo>/SKILL.md` emission describing the
  snapshot and how to regenerate it.
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
2. **Browse** to a source folder — or type `owner/repo` / a GitHub URL to pack a remote
   repo (optional PAT field appears; it stays in memory only).
3. Adjust options (respect gitignore, redact secrets, tree/TOC, formats, compression,
   git context, hidden files, virtual environments).
4. Optionally **Scan & curate** first: untick files/folders in the tree, click tiles in
   the token treemap, or rescue skipped files ("include anyway"). The Merge button then
   merges the selection.
5. Optionally **Change** the output path (a folder auto-names the file; a file path is used
   verbatim).
6. **Merge** (or toggle **Watch** to re-merge on every change), then **Open File** or
   **Show in Folder**.

### CLI

```text
turbomerger merge <src|owner/repo|URL> [out]
    [--format md|xml|cxml|json|plain] [--ordering path|entry-first|important-last]
    [--max-tokens N] [--include GLOB] [--exclude GLOB]
    [--compress] [--strip-comments] [--git-diff] [--git-log N] [--emit-skill]
    [--no-redact] [--no-gitignore] [--include-hidden] [--include-venv] [--quiet]
turbomerger map <src|owner/repo|URL> [out] [--tokens N]
turbomerger mcp     # stdio MCP server
```

Remote refs shallow-clone to a temp dir (private repos: set `TURBOMERGER_PAT`).

### MCP (Claude Desktop / Claude Code)

```json
{ "mcpServers": { "turbomerger": { "command": "C:/path/to/turbomerger.exe", "args": ["mcp"] } } }
```

Tools: `pack_directory` (merge → file, summary returned), `repo_map` (map text inline),
`read_output` / `grep_output` (sliced access to `*_merged.*` outputs only).

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
src/                     React/TypeScript UI (App.tsx + components/CuratePanel.tsx)
src-tauri/src/
  lib.rs                 Tauri entry point + command registration
  main.rs                CLI dispatch (merge / map / mcp) before the GUI starts
  commands.rs            IPC handlers (merge, scan, watch, remote, repo-map) + CLI
  config.rs              turbomerger.toml loader
  scanner/mod.rs         gitignore-aware walk + text/binary classification
  security/mod.rs        path validation, binary detection, secret redaction
  merger/mod.rs          parallel read/decode/redact + multi-format writer + report
  compress/mod.rs        tree-sitter signatures-only compression + comment strip
  repomap/mod.rs         def/ref tags → PageRank → budgeted signature map
  remote/mod.rs          owner/repo & URL parsing + shallow clone (temp, self-cleaning)
  mcp/mod.rs             stdio MCP server (pack/map/read/grep tools)
src-tauri/tests/         end-to-end fixture-tree tests
```

## License

MIT — see [LICENSE](LICENSE).
