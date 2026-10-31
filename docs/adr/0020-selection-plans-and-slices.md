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
