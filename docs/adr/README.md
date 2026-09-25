# Architecture decision records

One file per decision (Michael Nygard format): context, decision, consequences,
status. Status is **Proposed** until the maintainer confirms it; **Accepted**
decisions are binding for every AI or human session; **Superseded** ones link
to their replacement.

| ADR | Decision | Status |
|---|---|---|
| [0001](0001-record-decisions.md) | Record decisions as ADRs | Accepted |
| [0002](0002-platforms-longevity-distribution.md) | Platforms, Class-2 longevity, distribution, signing (q1 q2 q5 q9) | Accepted |
| [0003](0003-cpu-first-acceleration.md) | CPU-first, calibrated acceleration (q3) | Accepted |
| [0004](0004-single-trunk.md) | Single trunk + tags (q4) | Accepted |
| [0005](0005-document-bundles.md) | Documents become bundles; renderer chosen by a gate (q6 q15) | Accepted |
| [0006](0006-offline-by-default.md) | Network off by default; no Python in the product (q7 q8) | Accepted |
| [0007](0007-redaction-policy.md) | Redaction for documents; known-value masking (q10) | Accepted |
| [0008](0008-media-and-ocr-models.md) | Photos in Phase 6, audio/video in Phase 7, OCR model bundling (q11 q12) | Accepted |
| [0009](0009-token-targets.md) | Token counting per target (q13) | Accepted |
| [0010](0010-applyback-control-paths.md) | Apply-back control-path policy and handle-relative writes (q14) | Accepted — implemented |
| [0011](0011-exit-codes.md) | CLI exit-code contract | Accepted — implemented |
| [0012](0012-private-fixtures.md) | Private and licensed fixtures stay local | Accepted — implemented |
| [0013](0013-phase1-secret-masking.md) | Phase-1 secret masking: one deterministic pass, no prose harvest | Accepted — implemented |
| [0014](0014-mcp-confinement.md) | MCP root confinement and version negotiation; rmcp SDK in Phase 2 | Accepted — implemented |
| [0015](0015-two-binaries.md) | Two binaries: `turbomerger` (console CLI) and `turbomerger-gui`; workspace layout | Accepted — implemented |
