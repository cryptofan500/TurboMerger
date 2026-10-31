# 0021 — TM-GRID/1: spreadsheets as text that keeps every cell in its column

- **Status:** Accepted (2026-10-31, session 8) — implemented in session 7 (`74c1f15`)
- **Context:** Several Excel workbooks merged into one Markdown file lost alignment: empty
  cells collapsed, line breaks inside cells split rows, numbers stored as text came back
  as numbers, and nothing showed that a sheet had been cut off.
- **Decision:** a workbook, or a CSV/TSV with a table selection, becomes a `tm-grid`
  block: a `TM-GRID/1` first line, then per sheet `@sheet …` meta, GFM tables with a
  row-number column, `'` before text that looks like a number, `<br>` for an in-cell line
  break, and `@end … sha256=…` over the sheet's cells; a sheet without `@end` was cut.
  Skipped sheets are listed, not dropped silently. Grids are data: no slimming flag
  (`--remove-empty-lines`, `--truncate-base64`) touches them.
- **Consequences:** `tm_verify.py grid` decodes every block and compares each cell (type
  and text) with an independent reader (openpyxl); `cold` rebuilds the workbooks from the
  Markdown alone in an empty folder. In an archive twin a workbook is `binary` (base64).
