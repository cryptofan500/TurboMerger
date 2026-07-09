# Changelog

All notable changes to TurboMerger will be documented in this file.

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
