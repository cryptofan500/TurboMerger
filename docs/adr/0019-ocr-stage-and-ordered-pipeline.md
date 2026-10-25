# 0019 — OCR as a batched, cached stage; an ordered pipeline without chunk barriers; figures kept beside the output

- **Status:** Accepted (2026-10-25, session 6) — implemented on
  `feature/large-input-doc-extraction` (`8ef7392` the stage and the pipeline; session 6 the figures)
- **Context:**
  - Before `8ef7392`, every picture and every text-less PDF page started its own
    `monaddoc` process while the text merged: a model load (seconds) per picture, the
    same photo read again for each copy and for its page inside a scanned PDF, and
    nothing remembered between runs. `pdf_to_markdown_claude` with OCR on was
    estimated at ~1,200 reads ("32 h"); its unique photos number 149.
  - Merging in fixed chunks made every chunk wait for its slowest file: a 507 MB
    text file held the whole merge for 38.5 s while the pool idled.
  - monaddoc cuts figures out of pages and links them from its Markdown
    (`![Figure 1 on page 2](<stem>_assets/p002-1.webp)`). The merge dropped those
    links, so a photographed page lost its pictures. The maintainer asked for the
    NSK approach "with or without figure preservation, unticked by default".
- **Decision:**
  - `extract::OcrQueue` is the merge's OCR stage. While the text merges, pictures
    and the text-less pages of PDFs are queued (each page by its own JPEG when the
    PDF holds one: `pdf_oxide` page image handles, so `NSK.pdf` reuses the readings
    of its source photos), deduplicated by the SHA-256 of their bytes, and read after
    the text in batches of up to 64 pages per engine start (`TURBOMERGER_OCR_BATCH`).
    Every reading is kept in `<user cache>/turbomerger/ocr-v1/<sha256>-<engine>.json`
    (`TURBOMERGER_OCR_CACHE`, `off`), so a second run reads nothing it read before and
    a cancel loses at most the batch in flight. Engines run in their own process group
    (`PR_SET_PDEATHSIG`; a Job Object on Windows): Cancel kills the group at once.
  - Skip rules, with OCR on: thumbnails and icons (longest side ≤ 320 px),
    `NAME_assets/` crops beside `NAME….md`, and MonadDoc work folders are listed as
    excluded, not read; `--ocr-all-pictures` reads them. Sizes are by dimensions, not
    bytes (a crop of one text line can be 8 KB).
  - The merge runs as an ordered pipeline (`merger/pipeline.rs`): a worker takes the
    next file as soon as it is free, under a weight budget (`TURBOMERGER_INFLIGHT_MB`,
    256 MB; documents count ×4), and results are written in path order. Big streamed
    text counts tokens on ≤ 8 plain low-priority threads, never on the rayon pool
    (pool workers can all be parked waiting for the merge: a deadlock).
  - Progress: a `Stage::Ocr` with pages done / total, no ETA before 3 items of a
    stage, the finish clock names the day when it is not today, "(rough)" on a band
    ≥ half the estimate, monaddoc's "waiting for memory" line shown as the status
    ("close other programs, or Cancel"), and the stall line names the file waited on
    longest.
  - **Figures (session 6):** `--keep-figures` (desktop "Keep figures (with OCR)";
    off by default; never set by the MCP server) keeps monaddoc's crops. The stage
    copies each crop beside the output as `<output stem>_assets/<picture sha256, 12
    hex>-<page>-<n>.webp` and rewrites the page's link to it; `--max-tokens` parts
    share the one folder; the Merge Report says "Figures kept: N in `…_assets/`".
    Cache entries with figures are separate (`…-fig.json` + `…-fig.assets/`), so
    text-only output is byte-identical to before and a figure run restores its crops
    from the cache without starting the engine. Only monaddoc makes crops; with
    `$TURBOMERGER_OCR_CMD` the option keeps nothing. A link whose crop is missing is
    dropped rather than left dangling. monaddoc 0.5 writes the crops for
    `--only md-structured` (0.4 only linked them).
- **Consequences:**
  - `pdf_to_markdown_claude` with OCR on: 149 readings instead of ~1,200; 30 min 06 s
    on this laptop for the first run, seconds for the second. 507 MB text file: 38.5 s
    → 11.7 s; arXiv 6.67 → 5.99 s; outputs hash-identical old vs new.
  - Benchmarks must run with `TURBOMERGER_NO_CACHE=1` and a fresh
    `TURBOMERGER_OCR_CACHE`: warm caches fake up to 8× speed-ups.
  - A text-less page with no picture is reported "blank (no text, no picture)" and
    not sent to OCR (text drawn as outlines needs page rendering, ADR 0005).
  - Figure crops cost disk beside the output and in the cache (WebP, a few KB each);
    the option stays off by default.
