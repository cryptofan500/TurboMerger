# 0017 — Everything by default: redaction off, gitignored files in, no size cap

- **Status:** Accepted (2026-10-20, the maintainer's instructions in the 2026-10-19/20
  sessions) — implemented on `feature/large-input-doc-extraction` (`83c1cf5`, `0ab49fa`)
- **Context:**
  - The maintainer's main complaint: "won't have all text on large bodies". Measured
    causes (handoff 2026-10-20 §1.5): documents reported as not captured, a 2 MB (then
    50 MB) per-file cap, gitignored files left out, and redaction that masked research
    text. On the residency folder redaction produced 14,495 masks for 3 real passwords;
    one rule (four four-letter words read as an app password) caused 79 % of them, and
    redaction cost up to 44 % of CPU.
  - Output is for the maintainer's own LLM sessions first; sharing is the exception.
- **Decision:**
  - Secret redaction is **off** by default in the CLI and the desktop app (one-time
    settings migration v1 → v2); `--redact` / **Redact secrets** turns it on. The MCP
    server keeps it forced on (ADR 0014): a client must never get an unredacted dump.
  - Files hidden by `.gitignore` / `.ignore` / `.turbomergerignore` are **merged** by
    default; `--gitignore` / **Respect .gitignore** restores the old output byte for
    byte. Build output stays out by markers, not names (`target/` next to a
    `Cargo.toml`, folders tagged `CACHEDIR.TAG`), plus dependency folders, VCS metadata
    and virtualenvs, each listed in the report. Folders named `build`, `dist`,
    `release`, `debug` are not pruned by name: v7.7.0 lost real sources that way.
  - **No per-file size cap** by default; big files stream in bounded memory.
    `--max-file-size MB` still sets one.
  - Hidden files, `.env` included, stay out by default and are recorded
    (`--include-hidden`).
- **Consequences:** Merged output can contain secrets; the README, SECURITY notes and
  CHANGELOG say so, and say to use `--redact` for anything shared. Gitignored build
  bundles without a marker (a minified Vite `dist/`) are merged; whether to record
  minified files instead is an open question for the maintainer (handoff ❓ f).
  Precise redaction rules for when it is on remain Phase 5 (ADR 0007).
- **Supersedes:** the 50 MB default of Phase 3 part 1 (CHANGELOG) and the earlier
  redaction-on and gitignore-on defaults of the CLI and the desktop app.
