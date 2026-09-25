#!/usr/bin/env bash
# A.2 — v7.7.0: merged=2 scan_skipped=4 merge_skipped=2, exit 0; 7 files silently lost;
# cxml output has a forged 4th <document>; header carries the absolute source path.
. "$(dirname "$0")/common.sh"
rm -rf "$SP/fx"; bash "$here/a02_build_fixture.sh" "$SP/fx"
set +e
"$TM" merge "$SP/fx" "$SP/fx_out.md"; echo "exit=$?"
"$TM" merge "$SP/fx" "$SP/fx.xml" --format cxml -q; echo "exit=$?"
grep -c '<document index=' "$SP/fx.xml"
grep -n '^> Source:' "$SP/fx_out.md"
