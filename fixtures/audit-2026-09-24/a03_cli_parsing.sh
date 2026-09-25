#!/usr/bin/env bash
# A.3 — v7.7.0: a typo'd flag becomes the output file name; --max-tokens abc is
# accepted silently; --version without a display aborts in GTK (exit 134).
# Fixed: usage errors exit 2 and write nothing; --help/--version work headless.
. "$(dirname "$0")/common.sh"
[[ -d "$SP/fx" ]] || bash "$here/a02_build_fixture.sh" "$SP/fx"
mkdir -p "$SP/cwdtest"; cd "$SP/cwdtest"
set +e
"$TM" merge "$SP/fx" --includ-hidden -q; echo "exit=$?"; ls -la
"$TM" merge "$SP/fx" out.md --max-tokens abc; echo "exit=$?"
env -u DISPLAY -u WAYLAND_DISPLAY "$TM" --version; echo "exit=$?"
