# 0009 — Token counting per target

- **Status:** Accepted (2026-09-24, confirmed by the maintainer; plan §18 q13, §14)
- **Decision:** o200k counts are exact. Gemini counts are exact offline once the optional
  Gemma-3 tokenizer is downloaded (not bundled; Gemma terms). Claude counts are o200k ×
  a per-content-class `[low, high]` factor; packing uses `high`. Optional calibration
  uses the maintainer's own API key and never sends bundle content without a flag.
