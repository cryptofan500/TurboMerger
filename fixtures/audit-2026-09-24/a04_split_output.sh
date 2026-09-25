#!/usr/bin/env bash
# A.4 — 6 x ~1.7k-token files, --max-tokens 3000. v7.7.0: 6 parts; part 1's tree
# lists 1 file; the skip list repeats in all 6 parts. Fixed: full tree + TOC with
# part numbers in part 1; report once; every part within budget incl. overhead.
. "$(dirname "$0")/common.sh"
rm -rf "$SP/split"; mkdir -p "$SP/split/src"
for i in 0 1 2 3 4 5; do
  python3 -c "print('\n'.join('fn f${i}_%d() { let v = %d * 3 + 1; println!(\"{}\", v); }' % (j, j) for j in range(90)))" > "$SP/split/src/f$i.rs"
done
printf '\x89PNG\r\n\x1a\n\0\0' > "$SP/split/logo.png"
set +e
"$TM" merge "$SP/split" "$SP/split_out.md" --max-tokens 3000; echo "exit=$?"
grep -c '^### Skipped files' "$SP"/split_out*.md
