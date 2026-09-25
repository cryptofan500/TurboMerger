#!/usr/bin/env bash
# A.6 / N-35 — v7.7.0 on 200 x 100 KB: 0.71 s -> 5.97 s (2,000 tokens) -> 21.65 s
# (8,000 tokens), single-threaded after "100 %". Fixed: dense-file cost ~flat.
. "$(dirname "$0")/common.sh"
for n in 2000 8000; do
  rm -rf "$SP/perf$n"; python3 "$here/a06_build_perf.py" "$SP/perf$n" 200 "$n"
  for v in corpus_base corpus_dense; do
    /usr/bin/time -f "$v tokens=$n wall=%es cpu=%P" "$TM" merge "$SP/perf$n/$v" "$SP/perf_${n}_$v.md" -q || true
  done
done
