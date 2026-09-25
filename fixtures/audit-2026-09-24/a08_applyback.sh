#!/usr/bin/env bash
# A.8 — v7.7.0: apply-back writes through a symlink to a file outside the root, and
# rewrites a Windows-1252 file as UTF-8 (line 1 "café" -> "caf" + U+FFFD).
# Fixed: the symlinked target is refused; the cp1252 file keeps its encoding.
. "$(dirname "$0")/common.sh"
rm -rf "$SP/ab" "$SP/enc"
mkdir -p "$SP/ab/root/docs" "$SP/ab/outside"
printf 'ORIGINAL OUTSIDE CONTENT\n' > "$SP/ab/outside/target.txt"
ln -sfn ../../outside/target.txt "$SP/ab/root/docs/note.md"
printf '## docs/note.md\n\n```md\nWRITTEN BY APPLY-BACK\n```\n' > "$SP/ab/reply.md"
set +e
"$TM" apply "$SP/ab/root" --from "$SP/ab/reply.md" --yes; echo "exit=$?"; cat "$SP/ab/outside/target.txt"
mkdir -p "$SP/enc/root"
printf 'line one caf\xe9\nline two\nline three\n' > "$SP/enc/root/menu.txt"
printf -- '--- a/menu.txt\n+++ b/menu.txt\n@@ -2,2 +2,2 @@\n line two\n-line three\n+line THREE\n' > "$SP/enc/reply.md"
"$TM" apply "$SP/enc/root" --from "$SP/enc/reply.md" --yes; echo "exit=$?"; head -1 "$SP/enc/root/menu.txt" | xxd -p
