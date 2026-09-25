# 0013 — Phase-1 secret masking: one deterministic pass, no prose harvest

- **Status:** Accepted (2026-09-24) — implemented (findings N-13, N-14, N-35; bridge to ADR 0007 / Phase 5)
- **Context:** Repo-wide "propagation" of harvested secrets ran `str::replace` per
  secret per block from a `HashSet`: output differed between runs and could leak the
  tail of a longer secret (A.5), raw substring replacement cut inside numbers
  (`4,096-token` → `4,0[REDACTED]`), and on 120 papers 1,011 of 1,028 redactions were
  corrupting propagations from papers that merely tripped the credential-density rule.
  One 8,000-token notes file added 21 s of single-threaded work after "100 %".
- **Decision:**
  - All known values go into one Aho-Corasick automaton built from a sorted set;
    matches count only as whole tokens; overlaps resolve leftmost-longest; blocks are
    processed in parallel. Output is byte-identical across runs.
  - Unlabeled ("dense") tokens are harvested only from credential *dumps* — at least
    one line in ten is a credential indicator — and from credential files by name
    (`.env`, `*_CREDENTIALS_*.md`, credential stores). Long documents that trip the
    density rule donate nothing.
  - Dense tokens must be >= 12 characters and are never arXiv IDs or DOIs.
  - The frequency guard (a value in more than 8 blocks is a name, not a secret) now
    runs as one parallel automaton pass.
- **Not yet (Phase 5):** content classes (papers are still excluded wholesale by the
  density rule — N-07, reported as not captured), typed placeholders, the verifier.
