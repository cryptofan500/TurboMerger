#!/usr/bin/env bash
# A.12 — `pdftotext -layout` interleaves the two columns on every line; default
# extraction keeps reading order (critique §1.1 rejected). Usage: a12.sh <paper.pdf>
f="${1:?2608.31006.pdf}"
pdftotext -layout -f 2 -l 2 "$f" - | sed -n '10,18p'
echo '----- default -----'
pdftotext -f 2 -l 2 "$f" - | head -12
