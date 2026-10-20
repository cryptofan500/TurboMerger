#!/usr/bin/env python3
"""Compare the words TurboMerger extracted from each PDF with poppler's
`pdftotext` (an independent extractor) for the same file. Prints the
per-file word ratio (TurboMerger / poppler): min, median, the files below
0.9, and the total ratio over all PDFs.

Usage: python3 -I pdf_parity.py OUT.json SRC_ROOT
"""

import json
import os
import statistics
import subprocess
import sys


def file_of(path):
    if path.endswith(")") and " (part " in path:
        path = path.rsplit(" (part ", 1)[0]
    return path


def main():
    out, root = sys.argv[1], sys.argv[2]
    with open(out, encoding="utf-8") as fh:
        d = json.load(fh)
    words = {}
    for f in d["files"]:
        p = file_of(f["path"])
        if p.lower().endswith(".pdf") and "!/" not in p:
            words[p] = words.get(p, 0) + len(f["content"].split())
    rows = []
    for rel, tm in sorted(words.items()):
        r = subprocess.run(["pdftotext", os.path.join(root, rel), "-"],
                           capture_output=True, text=True, errors="replace")
        pop = len(r.stdout.split())
        rows.append((rel, tm, pop, tm / pop if pop else float("inf")))
    if not rows:
        print("no PDFs merged")
        return 1
    ratios = [x[3] for x in rows if x[2] > 0]
    tm_total = sum(x[1] for x in rows)
    pop_total = sum(x[2] for x in rows)
    print(f"PDFs compared: {len(rows)}")
    print(f"word ratio tm/poppler: min={min(ratios):.3f} median={statistics.median(ratios):.3f} "
          f"max={max(ratios):.3f}")
    print(f"TOTAL words: tm={tm_total:,} poppler={pop_total:,} ratio={tm_total / pop_total:.3f}")
    low = [x for x in rows if x[3] < 0.9]
    print(f"files below 0.9: {len(low)}")
    for rel, tm, pop, ratio in low[:20]:
        print(f"  {ratio:.3f}  tm={tm} poppler={pop}  {rel}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
