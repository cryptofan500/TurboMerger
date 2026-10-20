#!/usr/bin/env bash
# P4 check: one merge, fenced at 3 GB (no swap), while every thread's name,
# nice value and I/O priority is sampled from /proc. Prints the distinct
# (thread, nice, I/O) rows with their sample counts, the CPU time per
# thread kind, and /usr/bin/time's summary.
#
# Usage: thread_probe.sh BIN SRC OUT_DIR [extra merge args...]
# Writes OUT_DIR/{out.json,stdout.txt,stderr.txt,time.txt,samples.txt,rss.txt}.
set -u
BIN=$(readlink -f "$1"); SRC=$2; OUT=$3; shift 3
mkdir -p "$OUT"
: > "$OUT/samples.txt"
systemd-run --user --scope --quiet -p MemoryMax=3G -p MemorySwapMax=0 \
  /usr/bin/time -v -o "$OUT/time.txt" "$BIN" merge "$SRC" "$OUT/out.json" --format json -q "$@" \
  > "$OUT/stdout.txt" 2> "$OUT/stderr.txt" &
LAUNCHER=$!

# Find the merge itself (not systemd-run or time): its exe is BIN. Find only;
# nothing here kills by pattern.
PID=""
for _ in $(seq 200); do
  for p in $(pgrep -f -- "$OUT/out.json"); do
    [ "$(readlink -f /proc/"$p"/exe 2>/dev/null)" = "$BIN" ] && PID=$p
  done
  [ -n "$PID" ] && break
  kill -0 "$LAUNCHER" 2>/dev/null || break
  sleep 0.05
done
if [ -z "$PID" ]; then
  wait "$LAUNCHER"; echo "merge exited before it could be sampled (exit $?)"; exit 1
fi

# Field 19 of /proc/<tid>/stat is the nice value; fields 14/15 are user and
# system CPU ticks. Split after the last ')' because a name may hold spaces.
sample() {
  for t in /proc/"$PID"/task/*; do
    tid=${t##*/}
    comm=$(cat "$t/comm" 2>/dev/null) || continue
    rest=$(sed 's/.*) //' "$t/stat" 2>/dev/null) || continue
    read -r -a f <<< "$rest"
    io=$(ionice -p "$tid" 2>/dev/null | tr -d '\n')
    kind=${comm%%-[0-9]*}
    echo "$kind|nice=${f[16]}|io=$io|ticks=$(( f[11] + f[12] ))|tid=$tid"
  done >> "$OUT/samples.txt"
}
: > "$OUT/rss.txt"
while kill -0 "$PID" 2>/dev/null; do
  sample
  awk -v t="$(date +%s.%N)" '/^VmRSS/ {print t, $2}' /proc/"$PID"/status >> "$OUT/rss.txt" 2>/dev/null
  sleep 0.25
done
wait "$LAUNCHER"; CODE=$?

echo "merge exit: $CODE"
echo "-- distinct (thread kind, nice, I/O) rows, by samples:"
cut -d'|' -f1-3 "$OUT/samples.txt" | sort | uniq -c | sort -rn
echo "-- CPU seconds per thread kind (last sample of each thread):"
awk -F'|' '{k=$1; t=$5; sub("ticks=","",$4); last[t]=$4; kind[t]=k}
  END {for (t in last) s[kind[t]]+=last[t]; for (k in s) printf "%-14s %8.1f s\n", k, s[k]/100}' \
  "$OUT/samples.txt" | sort -k2 -rn
sort -k2 -n "$OUT/rss.txt" | tail -1 | awk '{printf "sampled peak RSS: %.0f MB\n", $2/1024}'
grep -E 'Elapsed|Percent of CPU|Maximum resident' "$OUT/time.txt"
cat "$OUT/stdout.txt" | tail -3
