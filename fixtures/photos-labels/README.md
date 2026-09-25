# photos-labels — 6 phone photos of shipping-label sheets (Phase 6 gate)

Spot test #2 corpus: six 12-MP iPhone HEIC photos of printed label sheets (header,
order table, 2–10 peel-off stickers in 1–2 columns, barcodes, a hand-written note,
thumbs over stickers, one tilted photo, stickers cut by the photo edge).

**The photos and the ground truth are private and never committed.** They show
real customer names, addresses, phone numbers and order numbers. Only these files
are tracked:

| File | Content |
|---|---|
| `MANIFEST.sha256` | sha256 of each photo — pins the exact bytes the gates were measured on |
| `expected.json` | the numeric acceptance gates (counts only, no strings) |
| `local/` (gitignored) | `photos/*.heic` and `truth.json` (134 check strings read by eye) |

## Populate `local/`

```bash
mkdir -p fixtures/photos-labels/local/photos
cp /path/to/turbomergerspottest/*.heic fixtures/photos-labels/local/photos/
(cd fixtures/photos-labels/local/photos && sha256sum -c ../../MANIFEST.sha256)
# truth.json: {"<photo stem>": ["check string", ...], ...}
```

Tests that need the payload skip with a message when `local/` is empty, so CI
stays green on machines without the private corpus.

## Oracle

`prototypes/photo_ocr_spottest.py` is the reference implementation. It reads
`local/truth.json` (or `$TM_PHOTO_TRUTH`) for the accuracy check only.

```bash
uv run prototypes/photo_ocr_spottest.py fixtures/photos-labels/local/photos "$SP/ph" --workers 3 --threads 2
uv run prototypes/photo_ocr_spottest.py fixtures/photos-labels/local/photos "$SP/b" --bench
```
