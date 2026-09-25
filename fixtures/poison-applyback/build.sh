#!/usr/bin/env bash
# Build the apply-back poisoning fixture (repros A.11 + A.8) under "$1".
#   $1/root      a git repo with an executable pre-commit hook and a symlink
#                docs/note.md -> ../../outside/target.txt
#   $1/outside   a file outside the root that a symlink points at
# Then:  turbomerger apply "$1/root" --from fixtures/poison-applyback/reply.md --yes
# v7.7.0: applies all eight proposals, exit 0 — overwrites the hook (mode kept rwx),
#         creates CI/IDE/agent auto-run files and build.rs, writes through the symlink.
# Fixed:  src/main.rs applies; control paths are refused unless --allow-control;
#         build.rs needs --allow-manifest; the symlinked target is refused; the
#         outside file and the hook are untouched; exit code 3 (applied with refusals).
set -euo pipefail
base="${1:?usage: build.sh <scratch_dir>}"
root="$base/root"
rm -rf -- "$root" "$base/outside"
mkdir -p "$root/src" "$root/docs" "$base/outside"
git -C "$root" init -q
printf 'fn main() {}\n' > "$root/src/main.rs"
printf '#!/bin/sh\necho original hook\n' > "$root/.git/hooks/pre-commit"
chmod +x "$root/.git/hooks/pre-commit"
printf 'ORIGINAL OUTSIDE CONTENT\n' > "$base/outside/target.txt"
ln -s ../../outside/target.txt "$root/docs/note.md"
echo "fixture ready: $root"
