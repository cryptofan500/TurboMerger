# arxiv-mini — 12 pinned arXiv papers (Phase 3 gate)

A 12-paper subset of spot test #1 (120 papers, 2026-08/09) chosen to cover every
failure mode the document pipeline must handle. `MANIFEST.tsv` lists each paper's
arXiv ID, path in the original corpus, size, **sha256**, page/figure/table counts
and why it was picked:

- REVTeX captions (`FIG. 1.`, `TABLE I.`) and two-column layouts
- 100+ pages; the slowest paper (14.9 s in the prototype)
- no PDF outline; figure-heavy; many full-page fallbacks
- duplicate labels (the `2608.31006` "Figure 4" case)
- referenced-but-not-detected figures
- a paper missing from `inventory.tsv` (title from PDF metadata)

**PDFs are never committed** — arXiv licences vary and most do not allow
redistribution. Populate `local/` (gitignored) with:

```bash
fixtures/arxiv-mini/fetch.sh --from ~/Desktop/arxiv   # copy from a local corpus
fixtures/arxiv-mini/fetch.sh --download               # or fetch from arxiv.org, >= 3 s apart
```

Both verify every file against the pinned sha256. A newer arXiv version shows up as
`MISMATCH` and is kept as `*.unverified`; re-baseline deliberately.

## Oracle

```bash
uv run prototypes/arxiv_spottest.py fixtures/arxiv-mini/local "$SP/arx" --workers 6
```

Phase 3 gates (full corpus): 120/120 processed, crop rate >= 97 %, 25-sample visual
QA 25/25, 0 silent drops, <= 60 s wall (goal 40 s), <= 1 GB RSS, byte-identical reruns.
