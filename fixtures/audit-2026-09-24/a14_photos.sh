#!/usr/bin/env bash
# A.14 / N-53 — a folder of HEIC photos. v7.7.0: "no text files found", exit 1,
# nothing written. Fixed (Phase 1): a report listing every photo as unsupported,
# exit 3. Phase 6 adds OCR. Usage: a14_photos.sh <photo_dir>
. "$(dirname "$0")/common.sh"
set +e
"$TM" merge "${1:?photo dir}" "$SP/photos_out.md"; echo "exit=$?"
ls -la "$SP"/photos_out* 2>/dev/null
