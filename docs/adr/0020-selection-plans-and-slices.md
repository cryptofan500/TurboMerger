# 0020 — Selection plans: merge exactly the planned files and parts of files

- **Status:** Accepted (2026-10-31, session 8) — implemented in session 7 (`74c1f15`), desktop in session 8 (`9e9969e`)
- **Context:** Web-LLM bundles need a known set of files and, inside big files, only the
  parts that matter (a dossier's summary, a script's scoring function, two sheets of a
  workbook). A folder walk with globs cannot say that, and a silent miss (a heading that
  was renamed) produced a bundle that looked complete and was not.
- **Decision:**
  - A plan is TOML (`*.tmsel.toml`, `version = 1`, `mode = "explicit"`): one `[[file]]`
    per path relative to the source folder, with optional `label`, `sections` /
    `exclude_sections` (`[own:][H<n>:]glob`), `lines`, `match` + `heading_context`,
    `max_tokens`, and for workbooks/CSV `sheets`, `columns`, `rows`, `header_row`,
    `[[file.sheet]]` overrides. Unknown keys, duplicate paths and `..` are errors.
  - Explicit mode never walks the folder. `on_missing` / `on_no_match = "error"` (the
    default) makes a missing file or a selector that matches nothing a "not captured"
    entry and exit 3 — never a quiet gap.
  - CLI: `--selection`, `--files-from` (`-0`), `--list-only`, `--emit-selection`,
    `--budget-tokens N` (exit 5 when over), `turbomerger outline FILE`. Desktop: "Load
    plan…" sends the parsed plan with the merge options; "Save as plan…" writes the
    curated list with the same writer (`SelectionPlan::to_plan_file`).
- **Consequences:** a bundle is rebuilt from its plan, never edited by hand; the
  independent verifier (`scripts/e2e/tm_verify.py slices`) re-cuts every slice in Python
  and compares bytes. Plans name personal files, so they live with the bundles, not in the repo.

## Addendum (2026-10-31, session 9): the archive twin (`--complete`)

- **Contract.** With redaction off, every source file of a `--complete` merge comes back
  byte for byte from the Markdown output alone: its text block rebuilds it (`kind: "text"`,
  checked by size and SHA-256 of the rebuilt text) or its bytes are embedded (`kind:
  "binary"`, a `## __binary__:` block). A file that cannot come back is named in the
  manifest (`kind: "withheld"` or `"missing"`, with `reason`), in the Merge Report
  ("Archive twin") and on stderr, and the CLI exits 3.
- **Which files.** Every file the merge was given, plus, in an explicit plan, every named
  file the scanner leaves out (a picture, a photo without OCR, an archive format TurboMerger
  cannot open). Continuation segments and archive members are output, not files, and get no
  manifest line.
- **Redaction.** With `--redact`, no source bytes are embedded: bytes the redactor never
  read (other sheets, metadata, a block that was redacted) must not ride along with a
  redacted text. A shareable redacted output and a byte-exact private archive are two
  different files.
- **Cap.** None by default; `--embed-binary MB` withholds (and names) a larger file.
- **Reads.** Pass 1 reads each file once, streamed (size, SHA-256, line ends, BOM, UTF-8),
  before anything is written; pass 2 streams the base64 and hashes again, and a file that
  changed in between fails the merge. An unreadable file is `missing`, never an empty file.
- **Format.** Markdown only; `--complete` with another format is a usage error (exit 2).
