# v8 refactor — progress ledger

Tracks the phases of the v8 refactor plan (plan v2 §17). Tick items as they land, so
progress survives across sessions. Each phase ends with the end-to-end protocol green
and its numbers in `fixtures/audit-2026-09-24/RESULTS_phase-<N>.txt`.

```
[x] Phase 0  fixtures · prototypes · ADRs                          2026-09-24
[x] Phase 1  N-01/02/03 · N-04/05(partial) · N-08/09 · N-12 · N-13/14/35 · N-15/17/52
             N-18/19(negotiation; SDK→P2) · N-20/21 · N-22/23/24 · N-30/31 · explain   2026-09-24
[x] L1.2 repo hygiene (security settings, Dependabot triage, draft deleted, metadata)   2026-09-24
[ ] L1.3 ship 7.8 (on hold by the maintainer)
[x] D1 macOS build   [x] D2 Windows run   [x] D3 GUI clicked through (fixtures/gui-smoke)
[x] Phase 2  2.1 [x] 2.2 [x] 2.3 [x] 2.4 [x] 2.5 [x] 2.6 [x] 2.7 [x] 2.8 [x] 2.9 [x] 2.10 [x] 2.11 [x] 2.12 [x]  gate [x]   2026-09-24
[ ] Phase 5  policy [ ] rules [ ] masking v2 [ ] N-07 [ ] placeholders+D9 [ ] verifier [ ] boundary tests [ ]  gate [ ]
[ ] Phase 3  tm-pdf [ ] renderer ADR [ ] bundle [ ] extractors [ ] quality gate [ ] tests [ ]  gate [ ]
[ ] Phase 6  toolchain bump [ ] tm-ocr [ ] HEIC ADR [ ] layout [ ] scanned PDFs [ ] synthetic fixture [ ]  gate [ ]
[ ] Phase 4  profiles [ ] packing v2 [ ] tokens per target [ ] digest/index [ ] skeleton+density [ ]  gate [ ]
[ ] Phase 7  calibrate [ ] media plugins [ ] converter hook [ ]  gate [ ]
[ ] Phase 8  CI matrix+weekly [ ] deny/audit/CodeQL [ ] fuzz [ ] artifacts+musl [ ] signing/attest/SBOM [ ] updater [ ]  gate [ ]
[ ] Phase 9  docs [ ] AGENTS.md [ ]  gate [ ]
```

Decisions: ADRs 0002–0014 accepted by the maintainer on 2026-09-24; 0015 (two binaries)
and 0016 (token counter, opt-level) added in Phase 2 (`docs/adr/README.md`).

Phase 2 gate (plan §17, handoff §5), measured 2026-09-24 — details in
`fixtures/audit-2026-09-24/RESULTS_phase-2.txt`:

| Gate | Result |
|---|---|
| 3.5 GB tree, bounded RSS, 0 files unaccounted | 3.4 GiB / 65,367 entries: 3.5 s, 119 MB peak RSS, 0 unaccounted |
| cancel < 1 s | CLI Ctrl-C 3–27 ms; GUI 0.12–0.52 s acknowledged; MCP and worker < 1 s (tests) |
| ETA shown after 60 s | Tracker tests; `--eta-after` (CLI, desktop) |
| stall message within 15 s of an injected hang | worker host: ~10 s with default limits (test) |
| Windows CLI prints and returns exit codes | CI step on windows-latest |
| musl CLI runs in Alpine | static-pie 19.7 MB, merges inside alpine:3 (local + CI) |
| Phase-1 tests and repros still green | yes (A.2 note in RESULTS) |
| golden outputs byte-identical | 17 runs, all three OSes in CI |
| A.6 ≤ 1 s, A.10 ≤ 1 s (release) | 0.58 s / 0.65 s |
