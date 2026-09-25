# big-tree — the Phase 2 scale fixture

A deterministic ~3.4 GiB synthetic tree (seed 42), generated on demand and
never committed: ~42,000 small text/code files, names that must not be pruned
by name (`packages/`, `build/`, `vendor/`, … — N-01), files at the 2 MiB size
cap, 200 MB of `node_modules`, a Rust `target/`, a gitignored `data/`, sparse
binary blobs (~0 bytes on disk), a `.git/`, and every kind of symlink.

```bash
SP=<scratch dir>
python3 fixtures/big-tree/gen.py build "$SP/bigtree/tree"           # ~17 s, ~430 MB on disk
/usr/bin/time -v target/release/turbomerger merge "$SP/bigtree/tree" "$SP/big.json" --format json
python3 fixtures/big-tree/gen.py verify "$SP/bigtree/tree" "$SP/big.json"   # exits 1 on any silent drop
```

`verify` accounts for every entry of `tree.inventory.tsv`: merged, skipped by
path, inside a directory that was not scanned, or covered by an ignore rule.

Gate (plan Phase 2): peak RSS ≤ 500 MB and 0 unaccounted entries. Measured on
2026-09-24 (Ryzen 5 5500U, release build): 4.7–5.8 s, 115–125 MB, 65,367
entries, 0 unaccounted.
