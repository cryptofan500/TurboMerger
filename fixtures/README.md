# Fixtures

Test corpora for the v8 refactor (plan: `TURBOMERGER_refactor_september24_2026_v2.md`, §17 Phase 0).

| Fixture | What | Tracked | Local-only payload |
|---|---|---|---|
| `audit-2026-09-24/` | shell repros A.1–A.14 + the v7.7.0 baseline | all | — |
| `poison-applyback/` | apply-back control-path / symlink poisoning (A.11, A.8) | all | — |
| `arxiv-mini/` | 12 pinned arXiv papers for the document pipeline | manifest, fetch script | PDFs (licences vary) |
| `photos-labels/` | 6 HEIC photos of label sheets for photo OCR | hashes, numeric gates | photos + ground truth (private data) |

`**/local/` is gitignored. Anything with personal data, customer data or a
restrictive licence lives there and never reaches the public repository.
Tests that need a local payload skip (with a message) when it is missing.

The Rust tests in `src-tauri/tests/` are the CI-enforced form of these repros:
they build equivalent trees in temp dirs, so they run on Windows, macOS and Linux.

The two prototypes in `prototypes/` are the reference oracles for the Rust
ports (Phase 3 documents, Phase 6 photos); they are not shipped.
