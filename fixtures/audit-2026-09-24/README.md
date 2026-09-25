# audit-2026-09-24 — black-box repros from the refactor plan

Shell versions of the repros in `TURBOMERGER_refactor_september24_2026.md`
(v1, Appendix A.1–A.10) and `…_v2.md` (A.11–A.14). They run against any
`turbomerger` binary, so the same script shows the v7.7.0 failure and the fix:

```bash
export SP=$(mktemp -d)                       # scratch
TM=/usr/bin/turbomerger  bash a05_determinism.sh   # v7.7.0
TM=src-tauri/target/debug/turbomerger bash a05_determinism.sh   # this tree
```

| Script | Repro | Finding(s) |
|---|---|---|
| `a01_arxiv_exit_code.sh <corpus>` | A.1 | N-06, N-31 |
| `a02_build_fixture.sh`, `a02_run.sh` | A.2 | N-01, N-02, N-04, N-20, N-21 |
| `a03_cli_parsing.sh` | A.3 | N-30 |
| `a04_split_output.sh` | A.4 | N-12 |
| `a05_determinism.sh` | A.5 | N-14 |
| `a06_build_perf.py`, `a06_perf.sh` | A.6 | N-35 |
| `a07_run_and_nfkc.sh` | A.7 | N-08, N-09 |
| `a08_applyback.sh` | A.8 | N-17, N-15 |
| `a09_mcp.sh` | A.9 | N-18, N-19 |
| `a10_arxiv_pdftotext.sh <corpus>` | A.10 | N-13 |
| `../poison-applyback/build.sh` | A.11 | N-52 |
| `a12_pdftotext_layout.sh <pdf>` | A.12 | critique §1.1 |
| `a14_photos.sh <dir>` | A.14 | N-53 |

A.13 (LaTeX source vs extracted-text tokens) was a one-off measurement that
downloads from arXiv; its numbers are recorded in the v2 plan and are not
re-run here.

The CI-enforced versions of these repros are Rust tests (`src-tauri/tests/`);
they build equivalent trees in temp dirs so they run on every OS.
`BASELINE_v7.7.0.txt` records what the installed v7.7.0 did on this laptop;
`RESULTS_refactor-phase-0.txt` records the same repros on the Phase 1 branch.
