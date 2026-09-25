# 0011 — CLI exit-code contract

- **Status:** Accepted (2026-09-24) — implemented (plan §10.7; findings N-31, N-53)
- **Context:** v7.7.0 exited 0 after dropping 120 of 122 inputs (a folder of PDFs)
  and exited 1 with nothing written for a folder of photos. Scripts and AI agents
  could not tell a complete merge from a hollow one.
- **Decision:** every skipped input carries a kind (`scanner::SkipKind`):
  `excluded` (on purpose: sensitive files, lockfiles, previous outputs, hidden files,
  user globs), `binary`, `not_captured` (content that should be in the output but is
  not: documents and photos without an extractor yet, too large, unreadable,
  credential-dense, a failed git section), `pruned_dir` and `ignored_by_rule`
  (summaries with counts).

  | Exit | Meaning |
  |---|---|
  | 0 | complete: everything found was merged or excluded on purpose |
  | 1 | error (no output) |
  | 2 | usage error (nothing written) |
  | 3 | completed, but something was not captured — or nothing was merged at all, or (with `--fail-on-skip`) any file was skipped; `apply`: some proposals were held or refused |
  | 4 | cancelled (Ctrl-C, `--deadline`, `--on-stall fail`; nothing written) or partial (`--on-deadline partial`, `--keep-partial`; what was done is written and the rest is listed as not captured) — implemented in Phase 2 |
  | 5 | reserved: verification failed (Phase 5 verifier) |

  When nothing can be merged but inputs were found, the output is still written and
  holds the report of what was skipped and why.
- **Consequences:** scripts that treated any non-zero exit as failure must accept 3
  where "report but continue" is wanted. The stdout summary gains `not_captured=N`
  at the end of its line (existing fields unchanged).
