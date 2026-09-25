#!/usr/bin/env bash
# A.10 / N-13 — merge the 120 papers as text. v7.7.0: merged=104 merge_skipped=16
# redacted=1028 (1,011 "Propagated known secret"). Fixed: 0 propagated edits.
# Usage: a10_arxiv_pdftotext.sh <arxiv_corpus_dir>
. "$(dirname "$0")/common.sh"
corpus="${1:?corpus dir}"
rm -rf "$SP/pdftxt"; mkdir -p "$SP/pdftxt"
find "$corpus" -iname '*.pdf' -print0 | xargs -0 -P 12 -I{} sh -c 'pdftotext -q "$1" "$0/$(basename "$1" .pdf).txt"' "$SP/pdftxt" {}
set +e
"$TM" merge "$SP/pdftxt" "$SP/pdf_corpus_merged.md"; echo "exit=$?"
grep -c 'Propagated known secret' "$SP/pdf_corpus_merged.md"
