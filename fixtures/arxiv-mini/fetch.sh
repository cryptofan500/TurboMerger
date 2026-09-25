#!/usr/bin/env bash
# Populate fixtures/arxiv-mini/local/ with the 12 pinned papers (MANIFEST.tsv).
#   fetch.sh --from DIR    copy from a local corpus laid out like ~/Desktop/arxiv
#   fetch.sh --download    fetch from arxiv.org (one request per paper, >= 3 s apart)
# Every file is verified against the pinned sha256; a mismatch (e.g. a newer arXiv
# version) is reported and the file is kept under local/ with a .unverified suffix.
# PDFs are never committed: arXiv licences vary and most do not permit redistribution.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
dest="$here/local"
mode="${1:-}"
src="${2:-}"
[[ "$mode" == "--from" && -n "$src" ]] || [[ "$mode" == "--download" ]] || {
  echo "usage: $0 --from <corpus_dir> | --download" >&2; exit 2; }
mkdir -p "$dest"
fail=0
while IFS=$'\t' read -r id rel bytes sha pages figs tabs abs why; do
  out="$dest/$rel"
  mkdir -p "$(dirname "$out")"
  if [[ "$mode" == "--from" ]]; then
    cp -- "$src/$rel" "$out"
  else
    curl -fsSL --retry 2 -A "TurboMerger-fixtures (research; low volume)" -o "$out" "https://arxiv.org/pdf/$id"
    sleep 3
  fi
  got="$(sha256sum "$out" | cut -d' ' -f1)"
  if [[ "$got" == "$sha" ]]; then
    echo "ok        $rel"
  else
    mv -- "$out" "$out.unverified"
    echo "MISMATCH  $rel (sha256 $got) — kept as .unverified" >&2
    fail=1
  fi
done < <(tail -n +2 "$here/MANIFEST.tsv")
if [[ "$mode" == "--from" && -f "$src/inventory.tsv" ]]; then
  # Title precedence (sidecar > PDF metadata > first-page heuristic) needs the rows.
  { head -1 "$src/inventory.tsv"; tail -n +2 "$here/MANIFEST.tsv" | cut -f2 | grep -F -f - "$src/inventory.tsv" || true; } > "$dest/inventory.tsv"
  echo "inventory.tsv: $(($(wc -l < "$dest/inventory.tsv") - 1)) rows"
fi
exit $fail
