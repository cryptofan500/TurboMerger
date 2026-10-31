# 0018 — A polite worker pool; big PDFs in an isolated worker with a memory budget

- **Status:** Proposed (2026-10-20, session 4) — implemented on
  `feature/large-input-doc-extraction` (`ca14d44`, `c884e10`, `8ffec63`)
- **Context:**
  - The maintainer asked for full use of the hardware without keyboard or mouse lag.
    Rayon's default pool used every core at normal priority.
  - `pdf_oxide` reads a PDF whole and clones it before parsing; it needs about 6.6 x
    the file size in memory (a 450 MB scan: 2.84 GiB). Under a 2 GB memory limit the
    whole merge was OOM-killed with no output. Release builds are `panic = "abort"`
    (ADR 0016), so no parser failure can be caught in-process.
  - Under systemd's default `OOMPolicy=stop`, a kernel OOM kill of any process in a
    scope stops the whole scope: making the worker the OOM victim is not enough.
- **Decision:**
  - `tm-core::threads` builds rayon's global pool once: logical cores − 1 (− 2 from 8
    cores), at least 1; `--threads N` / `TURBOMERGER_THREADS`. Each worker lowers its
    own priority (Linux nice 10, never raising a nicer one, and I/O best-effort 7;
    macOS QoS utility; Windows below-normal). Work that rayon would run inline on a
    non-pool thread (a one-item chunk) goes through `threads::on_pool`.
  - PDFs over 256 MiB parse in `<this binary> __worker pdf` (tm-worker protocol). The
    desktop binary forwards `__worker` to the CLI, so nothing extra ships. Memory
    budget: 80 % of what is available (`MemAvailable` and every cgroup v2 limit,
    counting inactive page cache as free), or `TURBOMERGER_PDF_WORKER_MB`. On Linux
    the host watches resident memory (`Limits::resident_bytes`): an `RLIMIT_AS` cap
    counts reserved virtual memory and refused a PDF that fits in RAM. Other Unixes
    keep `RLIMIT_AS` at 60 % of physical memory; Windows has no budget yet.
  - A worker over budget, crashed or timed out (60 s + 1 s per MB) makes that file
    "not captured" with the reason; spawn or protocol failure falls back to parsing
    in-process.
- **Consequences:** On a 12-core machine 10 workers run; the desktop keeps two cores
  and priority. All PDFs through workers measured +6 % time on 120 arXiv PDFs with
  identical output, so the next step is every PDF in a reusable worker pool, for crash
  isolation under `panic = "abort"` (Phase 3 part 2, maintainer's decision ❓ g).
  Files over 64 MiB still parse one at a time (Phase 9: a bounded in-flight window).

## Addendum S8 (2026-10-30): the release profile unwinds

- **Finding:** a byte-corruption fuzz of one arXiv PDF (120 variants, release build)
  aborted the merge twice (`pdf_oxide` 0.3.78: `document.rs:25363` char boundary,
  `document.rs:6540` subtract overflow; exit 134, no output at all). Truncation alone
  (5–99 %) never crashed it.
- **Decision:** `panic = "unwind"` in `[profile.release]`, plus a per-file
  `catch_unwind` in `merger/process.rs` around every file's processing, so a panic in
  any parser (PDF, workbook, Office XML, tree-sitter) costs that file only: it is "not
  captured" with the reason and the run finishes (exit 3). The PDF-only `catch_unwind`
  in `extract::pdf_parse` now works in release too. Out-of-memory and stack overflow
  still need the worker (> 256 MiB PDFs keep it); a reusable worker pool for every PDF
  stays open (❓ g).
- **Cost:** binary size before/after is in the S8 handoff (low-memory dev profile).
