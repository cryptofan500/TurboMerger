# Changelog

All notable changes to TurboMerger will be documented in this file.

## [7.4.0] - 2026-07-09

Agent + curation release — tree-sitter compression, repo map, a curate GUI,
watch mode, remote packing, an MCP server, and Claude-skill generation.

### Added
- **Compress to signatures** (`--compress` / Advanced toggle): tree-sitter elides
  function bodies (`{ ... }` / `...`) across rs/js/jsx/ts/tsx/py/go/java/c/cpp —
  signatures, types, imports, and class structure survive (~60–80% token cut).
  Independent **strip comments** toggle (`--strip-comments`). Both fail-safe: an
  unparseable file passes through unchanged (comment strip runs before compression —
  elided bodies aren't re-parseable source).
- **Repo map** (`turbomerger map <src> [out] [--tokens N]` + a `repo_map` app command):
  aider-style def/ref tags → file reference graph → PageRank → ranked signature map
  rendered to a token budget. The answer to "the whole repo won't fit".
- **Curate GUI** (Scan & curate): tri-state checkbox file tree with per-file token
  counts and a selected-vs-budget bar; a click-to-exclude token **treemap** (zoom into
  folders, hover for details); and a **skip-report drill-in** grouped by reason with
  per-file "include anyway" rescue (merge-level safety still applies to rescued files).
  Selection persists per project.
- **Watch mode**: re-merge (debounced 300 ms) on file changes into a stable
  `<repo>_watch_merged.<ext>` output; `.git` churn and own outputs are ignored.
- **Git context blocks** (`--git-diff`, `--git-log N`): working-tree diff (512 KB cap)
  and recent commits appended as final sections — secret-redacted like all content;
  "not a repo" becomes a report note, never an error.
- **Remote repo packing**: paste `owner/repo` or a GitHub/GitLab URL (GUI source field
  or CLI positional) — shallow clone into a self-cleaning temp dir, normal pipeline,
  PAT held in memory only (CLI reads `TURBOMERGER_PAT` env) and scrubbed from errors.
- **MCP server** (`turbomerger mcp`): stdio JSON-RPC 2.0 for Claude Desktop/Code —
  tools `pack_directory`, `repo_map`, `read_output`, `grep_output`. The read/grep tools
  only touch `*_merged.*` outputs, and MCP-driven merges force redaction on.
- **Claude-skill generation** (`--emit-skill` / Advanced toggle): writes
  `.claude/skills/<repo>/SKILL.md` (frontmatter, snapshot stats + output pointers,
  regenerate/map commands, project tree) into the scanned repo.
- **Full encoding pipeline** (completes D-2): UTF-16/BOM decode + chardetng legacy
  detection (windows-1252 …) with per-file decoding notes in the report.

### Changed
- Self-output exclusion now covers xml/json/txt outputs and split parts (was .md only),
  closing the non-markdown re-merge snowball.
- Deps: tauri 2.9.5 → 2.11.x (closes the advisory tracked as A-6), tiktoken-rs 0.12,
  tree-sitter 0.26 + 8 grammars, notify 8.2; frontend: ESLint 9 (flat config), Vite 7.

## [7.3.0] - 2026-07-09

Feature release — token awareness, output formats, and curation controls.

### Added
- **Real token counting** (tiktoken `o200k_base`) per file and per merge, with a
  Claude estimate (o200k × 1.18) and a context-fit hint (GPT 128k / Claude 200k / Gemini 1M).
- **Output formats**: Markdown (default), **Claude XML (cxml)**, XML, JSON, and Plain text —
  chosen in the UI or via `--format`. XML escapes content; cxml uses Anthropic's
  `<documents>` convention; JSON emits a structured `{files:[…], skipped:[…], tokens…}`.
- **Token-budget splitting**: set a max-tokens value and the output is split at file
  boundaries into `…part1-of-N` files, each headed "Part N/M — wait for all parts".
- **File ordering**: alphabetical, entry-points-first, or important-last (README/entry
  files at the end, where LLMs weight context most).
- **Include/exclude globs** (UI + `turbomerger.toml [filter]`) via ripgrep's override layer;
  content slimming (remove empty lines, truncate long base64 blobs).
- **Presets**: "LLM review (lean)", "Claude (cxml)", "Full archive", "Docs only".
- **Headless CLI**: `turbomerger merge <src> [out] [--format … --max-tokens … --exclude …]`
  for scripting and CI.
- **Quality-of-life**: drag-and-drop a folder onto the window; settings persist between
  runs; open-in-chat links (claude.ai / chatgpt.com / gemini); multi-part output list.

### Hardened (validated against a real credential-heavy repo, counts-only method)
- **Credential data files** (the `<NAME>_CREDENTIALS_<UTC>.md` convention, `passwords.csv`,
  `vault.txt`, `*.secrets.yaml`, …) are excluded wholesale and listed in the report, rather
  than relying on per-line redaction. Source files that merely mention the words
  (`password_reset.py`, `useApiKey.ts`) are unaffected.
- **Credential-dense files** (≥2 inline-credential indicators — login tables, Google
  app-passwords, key blocks) are excluded wholesale and reported.
- **Contextual redaction** of Google app-passwords and `email:password` values on
  credential-flavoured lines, leaving ordinary prose untouched.
- Result: in default (gitignore-respecting) mode the tested credential repo produced **zero**
  credential leaks (structured secrets, app-passwords, email:pass all zero). Free-form
  prose-embedded secrets in explicit `--no-gitignore` archive mode remain a documented
  limitation.

## [7.2.0] - 2026-07-09

Correctness, safety, and honesty release — plus a leaner codebase.

### Fixed (security / correctness)
- **Gitignored files no longer leak.** The scanner now honors `.gitignore` / `.ignore` /
  `.git/info/exclude` / `.turbomergerignore` (ripgrep's `ignore` crate). Previously a
  gitignored `profiles/` dir (warmed browser profile) was merged, leaking live
  `cf_clearance` cookies into the output.
- **Secret redaction actually runs.** `redact_secrets()` existed since v6 but had zero call
  sites; it's now wired into the merge path with a merged ruleset (adds OpenAI, Anthropic,
  GitLab, SSN, credit-card patterns that were in a never-loaded resource file), an entropy
  gate, and placeholder stopwords.
- **Legit files stop disappearing.** Sensitive-file matching moved from whole-path
  substrings (which silently dropped `password_reset.py`, `useApiKey.ts`,
  `config.environments.ts`, …) to filename-based rules.
- **Hidden config files are included** (`.gitignore`, `.mcp.json`, `.github/`,
  `.env.example`, `.eslintrc*`, …) instead of all dotfiles being invisible.
- One unreadable file/dir no longer aborts the whole scan.
- SQLite/DB journal extensions (`sqlite-wal`, `db-shm`, …) added to the binary set.
- CI never ran (`dtolnay/rust-action@stable` doesn't exist → `rust-toolchain`); Release
  pipeline modernized (`softprops/action-gh-release`, `contents: write`, no Cargo.toml
  regex rewrite).
- `vite.config.ts` used Tauri **v1** env-var names, so the app always built with a Safari
  target; now `chrome105`.

### Added
- **Merge Report** footer: every skipped file with a reason, redaction list, decoding notes,
  and a token estimate.
- **Token estimate + context-fit hint** (GPT 128k / Claude 200k / Gemini 1M).
- Collision-proof dynamic code fences (markdown-in-markdown no longer corrupts output).
- Linked table of contents; a genuinely recursive directory tree.
- UI options: respect .gitignore, redact secrets, include hidden dotfiles.
- `.turbomergerignore` support; UTF-8 BOM stripping; lossy-decode reporting;
  self-output (`*_merged.md`) and cloud-placeholder exclusion; reveal-in-Explorer.
- End-to-end integration test suite under `src-tauri/tests/`.

### Removed / leaned
- Dead code: `safe_open_file`, `is_within_root`, `detect_high_entropy_secrets`,
  backward-compat wrappers, the unsafe mmap read path, unreachable size limits.
- Dependencies: `jwalk`, `memmap2`, `lazy_static` (→ std `LazyLock`), `tauri-plugin-fs`
  (unused; its capability block was self-contradictory).
- Orphaned bundled `resources/*.json`, the Tesseract/OCR ghost feature, the v5 build
  script, and the stale `RESUME_INSTRUCTIONS.md`.
- Version strings now sourced from one place (`CARGO_PKG_VERSION` / `getVersion()`).

## [6.0.0] - 2026-01-10

### Added
- **Enterprise GUI** - Modern React interface with real-time progress bar, cancel button, and output file selection
- **Virtual Environment Auto-Skip** - Automatically detects and skips Python venv directories (500%+ faster on Python projects)
- **Lock File Exclusion** - Skips package-lock.json, Cargo.lock, yarn.lock, and other lock files that bloat LLM context
- **Security Hardening** - Path validation, symlink protection, and restricted filesystem access
- **Memory-Safe Streaming I/O** - Handles codebases of any size without memory exhaustion

### Improved
- **Performance** - Parallel directory walking with jwalk and multi-core merging with Rayon
- **Binary Detection** - 7-layer NuclearSieve pipeline ensures only text files are included
- **Skip Directories** - Expanded to 55+ directories including IDE folders, build outputs, and Windows system paths

### Security
- Strict Content-Security-Policy
- Symlinks are never followed (prevents directory traversal attacks)
- System paths blocked (Windows, Program Files, AppData)
- SSH keys and credential files excluded

### Technical
- Built with Tauri 2.0 and Rust
- React 18 + TypeScript frontend
- PHF compile-time perfect hash for extension lookups
- Streaming architecture for large file handling

## [5.1.0] - Previous Release

- Ultra-Efficient mode for 100k+ file datasets
- Performance optimizations for large-scale scanning
- IPC payload reduction from 30-100MB to ~500 bytes
