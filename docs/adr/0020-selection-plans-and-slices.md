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
- **Parts.** Each part's manifest carries its own files and starts with a header line
  `{"archive":"tm-twin/1","set":…,"part":k,"parts":n,"files":N}`; the set id is the
  SHA-256 of every manifest line. A restore needs every part, once, of one set.
- **Restore.** `turbomerger restore ARCHIVE… --dest DIR` indexes the parts (streamed),
  checks every entry (safe relative path, block present, no duplicate) and the existing
  files, decodes each file into a temporary file checked against the manifest, and moves
  them into place only when all pass. Never outside DIR, never through a symlink, never
  over a different file without `--overwrite`. `scripts/e2e/tm_verify.py restore` stays the
  independent second reader. Besides the bytes it restores the Unix permission bits
  (`mode`); not timestamps, owners, empty folders or symlinks (a symlink in the source is
  archived as the file it points to). Names that differ only in case, or that Windows
  refuses, are reported as warnings.

## Addendum (2026-11-07): a named file is wanted

- **Why.** An SSM selection named 253 files by exact path and got 250 with exit 0: its
  three `package-lock.json` were "lock/system file (context bloat) [excluded on
  purpose]". The explicit list ran the folder walk's bloat rules, and only a "not
  captured" entry fails a merge, so named files went missing with no failure: the quiet
  gap this ADR rules out. Decision D8 (2026-11-07): named = included.
- **Rule.** In explicit mode (`--files-from`, `--selection`, desktop "Load plan…") a
  named file is merged when its bytes are text or a document, whatever its name:
  - The walk's bloat rules do not apply. That covers lockfiles and system files by name
    (`package-lock.json`, `Cargo.lock`, `yarn.lock`, `go.sum`, `desktop.ini`, …), minified
    bundles (`*.min.js`, `*.bundle.js`), `turbomerger.toml` extension exclusions, and the
    pictures OCR skips as not worth reading. Extensions on the binary list only because
    they are build caches (`.lock`, `.map`, `.tsbuildinfo`, `.eslintcache`) go to the
    content check, so `.lockb` stays binary.
  - The safety rules still apply. A named file they hold back is **not captured**: exit 3,
    a Merge Report entry, and a `NOT CAPTURED` line in `--list-only`, with a reason that
    starts `named but withheld:`. The rules: a credential file while secrets are redacted
    (`.env`, key material, credential stores, also when a link with another name points
    at one); anything inside `.ssh/`, `.aws/` or `.gnupg/` (never scanned, redaction on
    or off); a previous TurboMerger output (`*_merged.md`, …). Paths outside the source
    folder (`..`, absolute, a symlink that resolves outside) and missing paths were
    already not captured. A withheld file never goes into the archive twin either:
    no text and no embedded bytes.
  - A named binary file has no text. Without `--complete` it is **not captured** ("named
    but not merged", exit 3). With `--complete` its bytes go to the archive twin (a
    `binary` entry, exit 0, `TWIN BYTES` in `--list-only`), as in the 2026-10-31 addendum.
- **Unchanged.** A folder walk keeps every rule as before: lockfiles, bundles and
  binaries are excluded on purpose, with exit 0. `--fail-on-skip` still turns any skipped
  file into exit 3.
- **Rescue** (desktop "include anyway", `MergeOptions::force_include`). It lifts the
  walk's rules for the files it names. It never lifts credential protection while
  redacting, nor a credential directory (finding F12): such a rescue is refused, and the
  file is not captured.
- **`--list-only`** runs the merge's own plan scan (`job::scan_plan`) and its exit rule:
  3 when a named file is not captured, and with `--fail-on-skip` when any file is skipped.
- **Walk fix.** In a walk, a symlink whose target is a credential file (while redacting)
  or lies inside a credential directory is left out. Before this, it carried that file in
  under its own name.
- **Cost.** Named lockfiles add tokens (a big `package-lock.json` can be 100 k or more).
  `--budget-tokens` (exit 5) bounds an output.
