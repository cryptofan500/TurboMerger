# 0023 — Pieces: `--max-tokens` is a contract, one file may span parts

- **Status:** Accepted (2026-10-31, session 9)
- **Context:** `--max-tokens N` packed whole files into parts; a file larger than a part
  went alone into one part, over the budget, with a warning. The complete PGY-1
  deliverables (2026-10-31) had parts of 3.1 MB and 8.6 MB for web windows of 180,000
  tokens because one workbook alone is ≈ 2.37 M tokens.
- **Decision:**
  - Before the parts are packed, a block larger than what a later part can hold is cut
    into pieces that each fit. Pieces are exact slices of the block's text, in order, and
    point into the same spool bytes (no copy). A cut goes before a TM-GRID `@sheet` or
    `@cont` line (each chunk restates its table header) or after a blank line when one is
    within the second half of the piece, else at a line end. Only a single line longer
    than a piece is cut inside it, at a character boundary.
  - A piece is a block named `path (piece k/n)`, or `path (piece k/n, line continues)`
    when it ends inside a line (the fence's added newline is then not part of the text).
    Piece 1 carries the file's notes and redaction counts; pieces 2…n are left out of the
    project tree.
  - Joining pieces 1…n in order gives the block's exact text: `tm_verify.py` joins them
    before every gate (`--merged` takes every part), `turbomerger restore` rebuilds a
    split text file from its pieces across parts, and the archive twin decides `text`
    from the whole text, before the split.
  - Every piece is counted exactly; one still over (token counts do not add up across a
    cut) is halved again. The only part that can still exceed the budget is part 1 when
    the tree, contents and report alone are larger (warning as before; `--no-tree` helps).
- **Consequences:** a part never holds more than N tokens of file text; a reader sees
  where a file continues; nothing is cut out. The archive twin's manifest and embedded
  bytes are still not counted in `--max-tokens` (a twin is for restoring, not for a web
  window; ADR 0020 addendum).
