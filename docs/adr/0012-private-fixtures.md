# 0012 — Private and licensed fixtures stay local

- **Status:** Accepted (2026-09-24) — implemented
- **Context:** The acceptance gates come from two real corpora: 120 arXiv PDFs
  (licences vary; most do not allow redistribution) and six phone photos of shipping
  labels (real customer names, addresses, phone and order numbers). The repository is
  public. The photo prototype embedded its ground truth as a Python literal.
- **Decision:** payloads live in `fixtures/**/local/` (gitignored). The repository
  keeps only manifests (sha256 pins), numeric gates, fetch scripts and READMEs. The
  photo prototype loads its ground truth from `fixtures/photos-labels/local/truth.json`
  or `$TM_PHOTO_TRUTH`. Tests that need a payload skip with a message when it is
  missing.
- **Consequences:** CI runs the synthetic fixtures only; corpus-level gates are run
  on the maintainer's machine. A newer arXiv version of a pinned paper shows up as a
  sha256 mismatch and must be re-baselined deliberately.
