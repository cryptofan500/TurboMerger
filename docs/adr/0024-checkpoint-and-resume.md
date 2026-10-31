# 0024 — Checkpoint and resume for long merges

- **Status:** Accepted (2026-10-31, session 10)
- **Context:** a merge built its whole output in one process; the spool was an unlinked
  temp file that died with it. A crash, an OOM kill, a reboot or a deadline at hour 6 of
  a multi-terabyte run lost hours 0–6 (HANDOFF_S8 4.3, DeepSeek A1, test plan B13).
- **Decision:**
  - `merge SRC OUT --checkpoint` keeps `OUT.tm-checkpoint/` beside the output: the spool
    as a named file and `journal.jsonl`, one line per finished source file — its size
    and modification time (ns), and per block the span in the spool, language, tokens,
    note, compressed/synthetic flags and the XXH3-64 of its text; or its skip reason.
    Streamed files (archives, big files) are one entry with all their segments.
  - `--resume` reopens it: every file whose size and modification time are unchanged and
    whose blocks still hash right is taken from the checkpoint (weight 0 for the workers,
    never read again); the rest is processed. The output is byte-identical to an
    uninterrupted run. The folder is removed when the outputs are in place, kept when the
    run ends partial (`--on-deadline partial`), cancelled or killed.
  - Journal lines are written only after the spool bytes they name were written: every
    2 s, before every streamed file, while the merge waits on a slow file, at the end of
    the text stage and on Ctrl-C. A torn last line is ignored. No `fdatasync`.
  - Line 1 is a fingerprint (version, the executable's size and time, the root, every
    content option); a checkpoint made otherwise is refused, never mixed in. A lock file
    (`File::try_lock`) keeps two runs off one checkpoint; `CACHEDIR.TAG` and a scanner
    rule keep the folder out of a merge of its parent.
  - Refused (exit 2): with `--redact` (the propagation pass needs every harvested secret;
    a journal of secret values on disk would be a leak), a remote source (temporary
    clone), no explicit OUTPUT. Files waiting for OCR are not journaled (the OCR cache
    makes their second read cheap).
- **Measured (S10, release build, 12 cores, 16 GB):**
  - 1,000,000 small files: killed with SIGKILL at 27.5 s with 589,115 files journaled;
    `--resume` 31.6 s against 54.9 s for a plain run; output identical (246.9 MB).
  - 120 arxiv PDFs: killed at 3.4 s (21 journaled), resume 5.7 s vs 6.8 s; identical.
  - 3.31 GiB mixed corpus (1 GiB log, 64 MiB line, archives, Office, PDFs): identical
    after kill + resume; the run was inside the 1 GiB log at the kill, which is redone —
    the unit of resume is one file.
  - Checkpointing an uninterrupted run costs no measurable time since the `fdatasync` was
    dropped (with it: +28.5 % on the 3.31 GiB corpus).
- **Consequences:**
  - The resumed run holds the journal in memory: about 600 B per journaled file
    (959 MB vs 605 MB peak at 1 M files). A compact or hashed index is the next step if
    tens of millions of files must resume.
  - A file is redone if the run stops inside it; splitting one huge file into resumable
    segments is not done.
  - Rebuilding TurboMerger between the stop and `--resume` changes the fingerprint: the
    checkpoint is refused and the run must start over with `--checkpoint`.
  - After a power loss (not a killed process) bytes a journal line names may be missing;
    the XXH3 check catches that and the file is read again.
