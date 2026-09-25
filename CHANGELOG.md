# Changelog

All notable changes to TurboMerger will be documented in this file.

## [Unreleased] — Phase 1 of the v8 refactor (targets 7.8.0)

Stops the silent content loss, corruption and write-anywhere paths found in the
2026-09-24 audit (`TURBOMERGER_refactor_september24_2026_v2.md`, findings N-xx).
Every fix ships with its repro turned into a test (96 → 156 Rust tests).

### Security
- **Apply-back never plants control files (N-52).** Files inside `.git` are
  refused outright (including `.GIT`, `.git.`, `git~1` and zero-width variants);
  CI pipelines, git-hook managers, editor and coding-agent settings, env loaders
  and toolchain configs are held until `--allow-control GLOB` or a per-file
  confirmation in the app; build manifests (`build.rs`, `package.json`,
  `setup.py`, `Makefile`, …) until `--allow-manifest GLOB`. Executable targets
  need `--allow-exec`; created files never get an executable bit.
- **Apply-back never writes through links (N-17).** All I/O goes through
  directory handles (cap-std 4.0.3, which includes the fix for
  GHSA-hp8f-xmx4-4qrg): a symlink or junction anywhere on a path is refused;
  replacements are temp-file + rename; the OS-reported path of every opened
  handle is checked against the root.
- **Restore only trusts backups this machine made.** A `.turbomerger/backups`
  tree shipped inside a cloned repo can no longer plant files; files edited
  since the apply are skipped, not clobbered.
- **MCP is confined (N-18, N-19).** `turbomerger mcp --root DIR` limits what
  clients can pack or map (default: the working directory, never `/` or the
  home folder); outputs always land in a managed folder (the client's `output`
  is only a file-name hint); read/grep serve that folder only; remote repos need
  `--allow-remote`; the protocol version is negotiated instead of echoed.
- **Output delimiters cannot be forged (N-20).** cxml content containing
  `</document` is XML-escaped and marked `escaped="xml"` (apply-back reverses
  it); plain-format delimiters carry a content-derived tag; control characters
  in file names are escaped; headers say contents are untrusted data.
  `--emit-skill` sanitizes the repo name and tree and is ignored for remote
  sources.
- **No absolute home paths in outputs (N-21)** — `Source: local folder "<name>"`
  unless `--show-source-path`.
- **Remote clones (N-22, N-23):** the token travels as an HTTP header through
  `GIT_CONFIG_*` (never argv or `.git/config`); Git LFS is skipped; clones time
  out after 5 minutes (`TURBOMERGER_CLONE_TIMEOUT`); a bare `owner/repo` is a
  local path — write `gh:owner/repo` to clone; `git@-o…` hosts are refused.
- **`--git-diff` covers only the merged files (N-24)** — sections for excluded
  files (credential-dense notes, deleted `.env`) are dropped and listed; git
  runs with `core.fsmonitor=false`, `--no-ext-diff`, `--no-textconv`.

### Fixed
- **Nothing vanishes silently (N-01, N-02, N-03).** `packages/`, `build/`,
  `debug/`, `release/`, `env/`, `vendor/`, `coverage/`… are merged unless
  `.gitignore` says otherwise; `target/` is pruned only next to a `Cargo.toml`,
  virtualenvs only with `pyvenv.cfg`/`conda-meta`, caches with `CACHEDIR.TAG`.
  Every pruned directory is listed with file and byte counts, every hidden file
  and symlink is listed, and ignored entries are counted per rule. Ancestor
  ignore files apply only inside a git worktree. Links to files inside the root
  are merged once. New `turbomerger explain <folder> <path>`.
- **Byte-identical output (N-14) and no text corruption (N-13, N-35).** Known
  secrets are masked in one deterministic, parallel Aho-Corasick pass, whole
  tokens only; unlabeled tokens are harvested only from credential dumps and
  credential files, never from long documents (the 120-paper corpus no longer
  gets 1,011 corrupting edits).
- **Apply-back keeps encodings (N-15):** Windows-1252, UTF-16 and BOM files
  round-trip byte-for-byte or are refused; characters the file's encoding
  cannot hold are refused instead of mangled.
- **Split outputs (N-12):** full tree and contents (with part numbers) in part
  1, the report once, and budgets that count headers and wrappers.
- **Text sniffing is validity-first (N-04, N-05):** UTF-8 text is never
  "binary" (`MZ-80 notes.md`, CJK subtitles, notebooks); `.cc .cxx .hh .cu
  .glsl .wgsl .csproj .sln .xaml .razor .qml .log .srt .vtt .eml .svg`… are text.
- **Linux `/run/media/…` and `/run/user/<uid>/…` are usable (N-08);** folder
  names are no longer NFKC-normalized before I/O (N-09).
- Outputs are written atomically (temp file + rename).

### Changed
- **CLI rebuilt on clap (N-30):** typos and bad values are usage errors (exit 2,
  nothing written); `--help`, `--version` and `completions <shell>` work without
  a display; `--git-log` no longer swallows the next argument; new `--config`,
  `--max-file-size`, `--show-source-path`, `--fail-on-skip`, `TURBOMERGER_*`
  environment mirrors.
- **Exit codes (N-31, N-53):** 3 = completed but content was not captured
  (documents and photos without an extractor yet, too large, unreadable,
  credential-dense) or nothing was merged — the report is still written.
  The stdout summary gains `not_captured=N`.
- The Merge Report groups entries into Not captured / Skipped files /
  Directories not scanned / Ignored by rules; the app shows the same groups and
  a "not captured" warning.

### Internal
- `fixtures/` (audit repros with the v7.7.0 baseline, apply-back poisoning,
  pinned arXiv and photo corpora with local-only payloads), `prototypes/`
  (reference oracles), `docs/adr/` (0001–0014).

## [7.7.0] - 2026-08-06

Linux release — TurboMerger runs natively on Linux x64 (Debian/Ubuntu/Mint and
compatible distros), joining Windows and macOS Apple Silicon under one version.

### Added
- **Linux x64 support**: native `x86_64-unknown-linux-gnu` build on the system
  WebKitGTK webview, packaged as a `.deb` (primary install path for
  Debian/Ubuntu/Mint) and a portable `.AppImage`, plus a platform-split bundle
  config (`tauri.linux.conf.json`) alongside the existing Windows/macOS ones.
- **Linux path policy**: reuses the existing `unix`-but-not-`macOS` system-root
  block list (`/usr`, `/bin`, `/sbin`, `/dev`, `/etc`, `/proc`, `/sys`, `/run`,
  `/boot`, `/lib`, `/lib64`) and the generic non-Windows symlink/reparse-point
  check; no code changes were needed here — it was already in place.
- **Open in file manager** (`xdg-open` via the `open` crate) behind Show in
  Folder on Linux; unlike Windows/macOS this opens the containing folder
  rather than pre-selecting the file, a documented `xdg-open` limitation.
- **CI on a real Linux runner**: GitHub Actions matrix extended to Windows x64 +
  macOS Apple Silicon + Linux x64 (`ubuntu-24.04`, matching the glibc/WebKitGTK
  baseline of current Debian/Ubuntu/Mint), running the full verify suite plus a
  no-bundle Tauri build on every push; the release workflow builds the `.deb`
  and `.AppImage` and folds them into the single-publisher draft release
  alongside the existing `SHA256SUMS.txt`.
- Onboarding + docs: `docs/LINUX.md` (install, build-from-source, smoke test,
  troubleshooting including the Ubuntu 24.04 `libfuse2t64` AppImage rename) and
  a `scripts/linux-check.sh` prerequisite checker mirroring the macOS one.

### Internal
- `docs/RELEASING.md` updated for a three-platform release checklist.
- `package.json` gains `tauri:build:linux`; version bumped to `7.7.0` across
  `package.json`, `src-tauri/Cargo.toml`, and `src-tauri/tauri.conf.json`
  per the existing version-agreement gate.

## [7.6.0] - 2026-07-13

macOS release — TurboMerger runs natively on Apple Silicon (M1–M4).

### Added
- **macOS Apple Silicon support**: native `aarch64-apple-darwin` build on the
  system WKWebView, ad-hoc-signed `.app` + `.dmg` bundle (macOS 12+), an
  `icon.icns` generated from the original TM logo, and platform-split bundle
  configs (`tauri.windows.conf.json` / `tauri.macos.conf.json`).
- **macOS path policy**: protected system roots (`/`, `/System`, `/Library`,
  `/usr`, `/private`, and broad `/Users` / `/Volumes` / `/Applications` roots)
  are rejected as scan roots, while real projects under `/Users/<name>/…`,
  external volumes, and safe temp descendants (`/private/tmp`,
  `/private/var/folders`) validate normally. Darwin's symlinked `/tmp`/`/var`
  aliases are handled: the selected root is rejected only if it is itself a
  symlink, then policy runs on the canonical path.
- **Open in Finder** (`open -R`) behind Show in Folder on macOS.
- **CI on real Apple hardware**: GitHub Actions matrix (Windows x64 + macOS
  Apple Silicon) running the full verify suite plus a no-bundle Tauri build on
  every push; the release workflow builds the NSIS installer and a verified DMG
  (arm64 slice + ad-hoc signature + `hdiutil` checks) and publishes a draft
  release with a single `SHA256SUMS.txt`.
- Onboarding + docs: `docs/MACOS.md` (M4 runbook, Gatekeeper guidance, smoke
  test), `docs/RELEASING.md`, `SECURITY.md`, a `scripts/macos-check.sh`
  prerequisite checker, and a `scripts/check-version.mjs` version-agreement gate.

### Fixed
- Frontend build target now follows the platform (WKWebView `safari13` on macOS
  instead of always Chromium `chrome105`); the broad `TAURI_*` env namespace is
  no longer exposed to client code.
- Watch mode ignores Finder metadata (`.DS_Store`) and TurboMerger's own
  `.turbomerger/` backup writes; drag/drop can no longer switch the source
  folder mid-watch; the Apply panel remounts when the selected root changes.

### Internal
- Rust pinned to `1.92.0` via `rust-toolchain.toml` (identical rustfmt/clippy
  locally, in CI, and on contributor Macs); `.nvmrc` Node 22; `.gitattributes`
  line-ending rules; Dependabot for actions/npm/cargo; GitHub Actions pinned to
  commit SHAs; `npm run verify` aggregate gate; new cross-platform path-policy
  and watch-filter tests.

## [7.5.0] - 2026-07-10

Apply-back release (T3-3) — the merge → chat → **apply the reply** loop closes.

### Added
- **Apply-back with visual diffs**: paste an LLM reply into the new **Apply** panel
  (or `turbomerger apply <root> --from reply.md [--yes]`). Parses three reply shapes:
  file headers (`## path`, `**path**`, `File: path`, backticked paths — TurboMerger's
  own markdown round-trips) followed by fenced blocks as whole-file replacements
  (new paths create files); TurboMerger's cxml documents pasted back; and unified
  diffs (fenced or bare) with drift-tolerant hunk placement and trailing-whitespace
  tolerance. Per-file **side-by-side diff** review with accept/reject checkboxes and
  +adds/−dels counts.
- **Backups + restore**: every apply first copies originals to
  `<root>/.turbomerger/backups/<UTC>/files/<rel>` plus a `manifest.json`;
  **Restore last apply** (UI button or `turbomerger apply <root> --restore`) reverses
  the newest apply, deleting files it created. `.turbomerger/` joined the always-skip
  set so backups never re-merge.
- **Safety rails** (all covered by tests): dry-run by default — parsing/previewing
  writes nothing; proposal paths are lexically confined to the target root (absolute,
  drive-qualified, `..`, and ADS `:stream` paths refused); binary targets refused;
  deletion diffs (`+++ /dev/null`) surfaced but never executed; per-file content-hash
  check between preview and apply fails files that changed on disk in the meantime;
  CRLF originals stay CRLF even when the reply is LF-only; chained changes to one file
  fold in reply order. The CLI prints paths + counts only (replies can embed secrets).

### Security
- **Labeled-value redaction** (new contextual rule): `password: X` / `token = Y` /
  `secret: Z` values (single- or double-quoted or bare) are redacted on any line when
  the value looks like a real secret (≥8 chars, letters *and* digits,
  entropy/special-char gate, stopword-immune). Code stays intact by design:
  identifier references (`token = userAccessToken`), env lookups, call expressions,
  type annotations, and placeholders never match.
- **Repo-wide known-secret propagation**: values learned from labeled lines and from
  credential files anywhere in the scan are redacted in **every** merged block. This
  closes the **prose-echo** class: changelogs, TODOs, and session notes that quote a
  password out of a credential file without any label on the line. Found by a
  differential source-vs-output containment test on two real credential-heavy repos
  (2026-07-10); structured-pattern scans alone cannot see this class. Reported per
  file as "Propagated known secret".
- **Credential-file harvest (gitignore-bypassing, harvest-only)**: credential documents
  (`.env`, `MASTER_CREDENTIALS_*.md`, `credentials.json`, …) are read to learn their
  secret values **even when gitignored** — they almost always are, so the normal scan
  never sees them, yet their values echo across the repo. Content is read, secrets
  extracted, content dropped: the credential file itself is **never merged** (the
  scanner still excludes it). This is what lets propagation scrub echoes of a
  gitignored secrets file.
- **Credential-dense document token sweep**: a file excluded for credential density
  (a login table, a keys doc) has *every* opaque token harvested — even values whose
  line grammar no labeled rule can parse — behind a frequency guard (a token in many
  blocks is a name/host, not a secret) so the merge is never shredded. Windowed
  opaque-token sweep does the same within credential-flavoured lines of merged files.
- **Env lookups added to stopwords** (`process.env`, `os.environ`, `getenv`,
  `import.meta`): the generic secret-assignment rule no longer mangles
  `secret: process.env.JWT_SECRET` style code — a pre-existing false positive the
  new tests exposed.

### Changed
- Merged outputs round-trip: feeding a TurboMerger markdown or cxml output back into
  the parser yields byte-identical proposals (golden-tested), so "apply the whole
  snapshot" is a no-op instead of a rewrite.

### Internal
- New `applyback` module (parser, hunk applier, `similar`-based preview diffs,
  backup/restore engine); 3 new Tauri commands (`preview_apply`, `apply_accepted`,
  `restore_backup`); scanner `find_credential_files` (gitignore-bypassing, harvest-only);
  +27 tests → 94 total (76 unit, 8 apply-back integration, 10 core integration).
  Mixed-line-ending round-trips (LF file with stray CRLF lines) are byte-exact — caught
  by release-exe E2E, locked in by tests. Credential safety verified end-to-end against
  two real credential-heavy production repos (app + support) by a counts-only differential
  source-vs-output containment harness: 0 of 36 source-extracted secret candidates
  survive into the default-mode output; no secret value ever printed.

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
