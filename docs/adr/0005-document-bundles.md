# 0005 — Documents become bundles; the renderer is chosen by a measured gate

- **Status:** Accepted (2026-09-24, confirmed by the maintainer; plan §18 q6, q15)
- **Context:** Spot test #1 (120 arXiv PDFs) proved a bundle layout — INDEX tree with
  title + path, DIGEST, per-document Markdown, cropped figures, contact sheets, parts,
  `manifest.json` — works for web-LLM critique.
- **Decision:** Document inputs produce that bundle. A typeset PDF is only available via
  an optional pandoc hook. The PDF renderer (pdf_oxide/tiny-skia vs PDFium) is picked
  by the Phase 3 gate on 200 sampled crops (visual QA >= 98 % equal, speed >= 0.8x).
