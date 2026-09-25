# 0016 — Keep tiktoken (ordinary encoding); build with opt-level 3

- **Status:** Accepted (2026-09-24) — implemented (Phase 2.11; findings N-38, N-39)
- **Context:** The plan proposed `bpe-openai` for o200k counts (N-38, "≈ 4×
  tiktoken, identical counts") and asked for a benchmark of `opt-level = 3`
  against `"z"` (N-39). Four release CLIs were measured on the laptop
  (Ryzen 5 5500U), median of 5 runs, peak RSS, 2026-09-24:

  | Build | A.6 base | A.6 dense | A.10 (120 papers) | big tree (3.4 GiB) | CLI size |
  |---|---|---|---|---|---|
  | tiktoken, `"z"` | 0.81 s | 0.81 s | 1.01 s | 5.41 s / 147 MB | 18.2 MB |
  | tiktoken, `3` | 0.61 s | 0.61 s | 0.81 s | 3.81 s / 150 MB | 19.6 MB |
  | bpe-openai, `"z"` | 1.01 s | 0.61 s | 0.81 s | 4.02 s / 178 MB | 46.7 MB |
  | bpe-openai, `3` | 0.61 s | 0.61 s | 0.61 s | 3.02 s / 182 MB | 48.0 MB |

  (These runs still carried a 0–200 ms exit delay from the CLI's progress
  thread, fixed in the same change; after the fix the chosen build measures
  0.58 / 0.55 / 0.65 / 3.48 s.)

  `bpe-openai` counts exactly what tiktoken's *ordinary* encoding counts (0
  differences on 232 files), but embeds a 32 MB serialized o200k table: +28 MB
  per binary (+140 %) and +30 MB RSS, for ~20 % on the largest tree and no
  gain on A.6. Token counting is not the bottleneck once work is parallel.
- **Decision:**
  - Keep `tiktoken-rs`, but count with `encode_ordinary`: a special-token
    string such as `<|endoftext|>` in a file is text, which is how a chat
    window tokenizes pasted content (v7 counted it as one special token).
  - Release profile `opt-level = 3`: 25–30 % faster for +8 % binary size.
- **Consequences:** Counts change only for files containing special-token
  strings. Revisit `bpe-openai` if it gains an o200k-only build with a smaller
  table, or if counting becomes the bottleneck (Phase 3 documents).
