# 0007 — Redaction policy for documents; known-value masking

- **Status:** Accepted (2026-09-24, confirmed by the maintainer; plan §18 q10; full design in plan §13, Phase 5)
- **Decision:** Documents, papers and prose get only high-confidence token formats and
  policy literals redacted — never digits inside numbers or citations. Known-value
  masking (the GitHub-Actions model) applies to code, with values taken only from
  excluded credential files, behind length/entropy/placeholder/identifier gates.
  Placeholders become typed and deterministic (`«SECRET:github_pat#1»`).
- **See also:** ADR 0013 for the Phase-1 bridge that ships before Phase 5.
