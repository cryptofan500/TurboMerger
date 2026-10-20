#!/usr/bin/env bash
# End-to-end check of the long-run question (CLI). Usage: gate_e2e.sh BIN WORKDIR
BIN=$1; W=$2
rm -rf "$W"; mkdir -p "$W/src"
for i in $(seq -w 1 12); do printf 'file %s line\n' "$i" > "$W/src/f$i.txt"; done
# One worker, 500 ms per file: ~6 s, so after 1 s the ETA is several seconds.
SLOW="RAYON_NUM_THREADS=1 TM_TEST_SLOW_FILE_MS=500"
ARGS="--eta-after 1s --confirm-over 1s --progress auto"

echo "### A: no terminal (stdin /dev/null) -> a note, never a prompt"
env RAYON_NUM_THREADS=1 TM_TEST_SLOW_FILE_MS=500 "$BIN" merge "$W/src" "$W/a.md" $ARGS </dev/null >"$W/a.out" 2>"$W/a.err"
echo "exit=$? written=$([ -s "$W/a.md" ] && echo yes || echo no)"; grep -E "long run|Proceed" "$W/a.err"

echo "### B: terminal, answer n -> cancelled, nothing written"
printf 'n\n' | timeout 60 script -qec "env $SLOW '$BIN' merge '$W/src' '$W/b.md' $ARGS" /dev/null >"$W/b.tty" 2>&1
echo "exit=$? written=$([ -s "$W/b.md" ] && echo yes || echo no)"; tr -d '\r' <"$W/b.tty" | grep -oE "This is a long run[^?]*\? \[Y/n\]|cancelled \([^)]*\)[^$]*" | head -3

echo "### C: terminal, answer y -> runs to the end"
printf 'y\n' | timeout 60 script -qec "env $SLOW '$BIN' merge '$W/src' '$W/c.md' $ARGS" /dev/null >"$W/c.tty" 2>&1
echo "exit=$? written=$([ -s "$W/c.md" ] && echo yes || echo no)"; tr -d '\r' <"$W/c.tty" | grep -oE "Proceed\? \[Y/n\]|merged=[0-9]+" | head -3

echo "### D: terminal, never answered -> finishes, does NOT hang (timeout 60 => exit 124)"
sleep 45 | timeout 60 script -qec "env $SLOW '$BIN' merge '$W/src' '$W/d.md' $ARGS" /dev/null >"$W/d.tty" 2>&1
echo "exit=$? written=$([ -s "$W/d.md" ] && echo yes || echo no)"; tr -d '\r' <"$W/d.tty" | grep -oE "finished before an answer|merged=[0-9]+" | head -3
