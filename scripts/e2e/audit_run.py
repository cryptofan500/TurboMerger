#!/usr/bin/env python3
"""Audit a `turbomerger merge --format json` output against the folder it
merged: every entry on disk that is not a folder must be accounted for, as
merged (in "files"), recorded (in "skipped"), or inside a recorded pruned
folder. Prints the counts and every unaccounted path; exit 1 if any.

Synthetic output paths map back to their file: `a.zip!/member` -> `a.zip`,
`big.log (part 2)` -> `big.log`.

Usage: python3 -I audit_run.py OUT.json SRC_ROOT
"""

import json
import os
import sys
from collections import Counter


def on_disk(root):
    """Every non-directory entry under root, as a POSIX relative path."""
    for dirpath, dirnames, filenames in os.walk(root, followlinks=False):
        # Symlinked folders are not walked; list them as entries.
        for d in list(dirnames):
            if os.path.islink(os.path.join(dirpath, d)):
                filenames.append(d)
                dirnames.remove(d)
        for name in filenames:
            full = os.path.join(dirpath, name)
            yield os.path.relpath(full, root).replace(os.sep, "/")


def file_of(path):
    """The on-disk file an output path came from."""
    if "!/" in path:
        path = path.split("!/", 1)[0]
    if path.endswith(")") and " (part " in path:
        path = path.rsplit(" (part ", 1)[0]
    return path


def main():
    out, root = sys.argv[1], sys.argv[2]
    with open(out, encoding="utf-8") as fh:
        d = json.load(fh)
    merged = {file_of(f["path"]) for f in d["files"]}
    skipped = {}
    pruned = []
    for s in d["skipped"]:
        if s["kind"] == "pruned_dir":
            pruned.append(s["path"].rstrip("/") + "/")
        else:
            skipped[s["path"]] = s["kind"]
    pruned.sort()

    def in_pruned(p):
        return any(p.startswith(pr) for pr in pruned)

    tally = Counter()
    unaccounted = []
    total = 0
    for p in on_disk(root):
        total += 1
        if p in merged:
            tally["merged"] += 1
        elif p in skipped:
            tally["recorded:" + skipped[p]] += 1
        elif in_pruned(p):
            tally["inside pruned folder"] += 1
        else:
            unaccounted.append(p)
    print(f"entries on disk: {total}")
    print(f"output: files_merged={d.get('files_merged')} skipped records={len(d['skipped'])} "
          f"(pruned folders {len(pruned)})")
    for k, v in sorted(tally.items(), key=lambda kv: -kv[1]):
        print(f"  {k:28} {v}")
    print(f"UNACCOUNTED: {len(unaccounted)}")
    for p in unaccounted[:50]:
        print("  " + p)
    return 1 if unaccounted else 0


if __name__ == "__main__":
    sys.exit(main())
