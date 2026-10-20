# 0005 — Documents become bundles; the renderer is chosen by a measured gate

- **Status:** Accepted (2026-09-24, confirmed by the maintainer; plan §18 q6, q15)
- **Implementation (2026-10-19, Phase 3 part 1 — text):** `crate::extract` extracts
  document *text* and the merger folds it into the normal block/redaction/token path
  (`Verdict::Document`). PDF text via `pdf_oxide` (reading-order aware, pure Rust);
  spreadsheets via `calamine`; `.docx`/`.pptx`/`.odt`/`.odp`/`.epub` via `zip` +
  `quick-xml`. No PDFium yet: the figure/table *rendering*, the renderer gate
  (pdf_oxide/tiny-skia vs PDFium), and the full bundle layout (INDEX/DIGEST/cropped
  figures/contact sheets/`manifest.json`) remain for Phase 3 part 2, which is also
  where PDF parsing should move into the `tm-worker` subprocess (release is
  `panic = "abort"`, so in-process `catch_unwind` cannot isolate a parser crash;
  `pdf_oxide` is validated at 100 % on 3,830 PDFs, so this is hardening, not a blocker).
  Scanned-PDF OCR is bridged by an opt-in external command (`--extract-images-text`)
  pending the Phase 6 `tm-ocr` worker (ADR 0008).
- **Context:** Spot test #1 (120 arXiv PDFs) proved a bundle layout — INDEX tree with
  title + path, DIGEST, per-document Markdown, cropped figures, contact sheets, parts,
  `manifest.json` — works for web-LLM critique.
- **Decision:** Document inputs produce that bundle. A typeset PDF is only available via
  an optional pandoc hook. The PDF renderer (pdf_oxide/tiny-skia vs PDFium) is picked
  by the Phase 3 gate on 200 sampled crops (visual QA >= 98 % equal, speed >= 0.8x).
