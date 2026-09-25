# v8 refactor — progress ledger

Tracks the phases of the v8 refactor plan (plan v2 §17). Tick items as they land, so
progress survives across sessions. Each phase ends with the end-to-end protocol green
and its numbers in `fixtures/audit-2026-09-24/RESULTS_phase-<N>.txt`.

```
[x] Phase 0  fixtures · prototypes · ADRs                          2026-09-24
[x] Phase 1  N-01/02/03 · N-04/05(partial) · N-08/09 · N-12 · N-13/14/35 · N-15/17/52
             N-18/19(negotiation; SDK→P2) · N-20/21 · N-22/23/24 · N-30/31 · explain   2026-09-24
[ ] L1.2 repo hygiene   [ ] L1.3 ship 7.8 (on hold)   [ ] D1 macOS build   [ ] D2 Windows run
[ ] Phase 2  2.1 [ ] 2.2 [ ] 2.3 [ ] 2.4 [ ] 2.5 [ ] 2.6 [ ] 2.7 [ ] 2.8 [ ] 2.9 [ ] 2.10 [ ] 2.11 [ ] 2.12 [ ]  gate [ ]
[ ] Phase 5  policy [ ] rules [ ] masking v2 [ ] N-07 [ ] placeholders+D9 [ ] verifier [ ] boundary tests [ ]  gate [ ]
[ ] Phase 3  tm-pdf [ ] renderer ADR [ ] bundle [ ] extractors [ ] quality gate [ ] tests [ ]  gate [ ]
[ ] Phase 6  toolchain bump [ ] tm-ocr [ ] HEIC ADR [ ] layout [ ] scanned PDFs [ ] synthetic fixture [ ]  gate [ ]
[ ] Phase 4  profiles [ ] packing v2 [ ] tokens per target [ ] digest/index [ ] skeleton+density [ ]  gate [ ]
[ ] Phase 7  calibrate [ ] media plugins [ ] converter hook [ ]  gate [ ]
[ ] Phase 8  CI matrix+weekly [ ] deny/audit/CodeQL [ ] fuzz [ ] artifacts+musl [ ] signing/attest/SBOM [ ] updater [ ]  gate [ ]
[ ] Phase 9  docs [ ] AGENTS.md [ ]  gate [ ]
```

Decisions: ADRs 0002–0014 accepted by the maintainer on 2026-09-24 (`docs/adr/README.md`).
