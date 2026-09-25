# 0002 — Platforms, longevity, distribution, signing

- **Status:** Accepted (2026-09-24, confirmed by the maintainer; plan §18 q1, q2, q5, q9)
- **Context:** The tool must keep working through ~2 years without maintenance on the
  three desktop OSes the maintainer uses, and be installable by others.
- **Decision:**
  - Targets: Windows, macOS, Linux desktop on x64 and ARM64, plus a static-musl Linux CLI.
  - Longevity: Class 2 — public, production quality, survives 2-year dormancy.
  - Distribution: GitHub releases first; then winget, Homebrew, Scoop, AUR (Flathub later).
  - Signing: free first — SignPath Foundation (apply) or unsigned + build attestations;
    paid certificates only if the maintainer chooses them.
- **Consequences:** Weekly scheduled CI, pinned toolchains, ML models mirrored in each
  release, fast-moving or restrictively licensed components kept outside the core binary.
