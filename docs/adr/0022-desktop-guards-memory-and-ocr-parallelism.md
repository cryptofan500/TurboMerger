# 0022 — Desktop guards for big merges, memory after a job, and one OCR engine at a time

- **Status:** Accepted (2026-10-31, session 8) — `9e9969e`, `95d7575`
- **Context:** One click of MERGE on a 5,758-file folder wrote one 531.6 MB file
  (179 M tokens); an editor froze opening it; the window kept 1.2 GB after the run.
  Reviews proposed OCR in parallel.
- **Decision:**
  - MERGE asks first when the estimate is over 1.5 M tokens or 50 MB (Curate first /
    Split into parts of 180,000 tokens / Merge anyway); outputs over 30 MB open in their
    folder by default.
  - The desktop binary caps glibc at two heap arenas (`mallopt(M_ARENA_MAX, 2)`, unless
    `MALLOC_ARENA_MAX` is set) and calls `malloc_trim(0)` after every job. Measured on the
    5,834-file folder (release): end of merge 1,358 MB → 477 MB, idle after 576 MB →
    141 MB, +9 % merge time. The CLI keeps mimalloc.
  - OCR stays one engine at a time: one monaddoc batch kept 8.4 of 12 cores busy; two
    engines read the same 24 photos 4.6 % slower with twice the memory.
- **Consequences:** a host with many more cores or a GPU may want parallel engines; that
  belongs to hardware means-testing (deferred). Pictures are staged beside the output.
