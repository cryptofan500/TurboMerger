# 0008 — Media phases and OCR model bundling

- **Status:** Accepted (2026-09-24, confirmed by the maintainer; plan §18 q11, q12 — bundle the small tier)
- **Decision:** Photos and OCR are Phase 6 (design proven by spot test #2: PP-OCRv6
  small, 133/134 check strings). Audio/video are Phase 7 and optional. GUI installers
  bundle PP-OCRv6 small (~32 MB, Apache-2.0); every tier is mirrored in each GitHub
  release and pinned by sha256 in `data/models.toml`. (Alternative: download on first use.)
