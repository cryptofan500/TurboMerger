#!/usr/bin/env python3
"""Synthetic ~3.5 GB tree for the Phase 2 scale gate (never committed).

    python3 fixtures/big-tree/gen.py build  <dir>          # deterministic (seed 42)
    python3 fixtures/big-tree/gen.py verify <dir> <out.json>

`build` writes the tree plus `<dir>.inventory.tsv` (every regular file and
symlink, relative path + size). `verify` reads a `turbomerger merge
--format json` output and accounts for every inventory entry: merged,
skipped by path, inside a directory that was not scanned, or covered by an
ignore rule. Anything else is a silent drop and fails the gate.

Contents:
- ~42,000 small text/code files in nested directories, including names that
  are source in many repos and must not be pruned by name (packages/, build/,
  debug/, release/, vendor/, env/, coverage/, out/, dist/, target/ without a
  Cargo.toml — plan N-01 / repro A.2);
- files just under and just over the 2 MiB size cap;
- 200 MB of node_modules-style content and a Rust target/ next to Cargo.toml;
- a gitignored data/ directory and ignored *.log files;
- sparse binary blobs (read back as zeros) to reach ~3.5 GB apparent size,
  some with a binary extension and some that must be content-sniffed;
- a .git directory; symlinks inside the root, escaping it, broken, and to a
  directory.
"""

import json
import os
import random
import sys
from pathlib import Path

SEED = 42
CAP = 2 * 1024 * 1024
EXTS = ["rs", "py", "ts", "tsx", "js", "go", "java", "c", "h", "md", "txt", "json", "toml", "yaml", "sh"]
WORDS = ("alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho "
         "sigma tau upsilon phi chi psi omega merge token scan redact spool chunk block part").split()
AMBIGUOUS = ["packages", "build", "debug", "release", "vendor", "env", "coverage", "out", "dist", "target"]


def text(rng: random.Random, size: int, ext: str) -> str:
    lines = []
    n = 0
    i = 0
    while n < size:
        w = " ".join(rng.choice(WORDS) for _ in range(rng.randint(3, 12)))
        if ext in ("rs", "go", "java", "c", "h", "ts", "tsx", "js"):
            line = f"fn f{i}() {{ let v = \"{w}\"; }}" if ext == "rs" else f"// {w}\nconst v{i} = \"{w}\";"
        elif ext == "py":
            line = f"def f{i}():\n    return \"{w}\""
        elif ext == "json":
            line = f"{{\"k{i}\": \"{w}\"}}"
        else:
            line = w
        lines.append(line)
        n += len(line) + 1
        i += 1
    return "\n".join(lines) + "\n"


def write(root: Path, rel: str, data, inventory):
    p = root / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    if isinstance(data, str):
        data = data.encode()
    p.write_bytes(data)
    inventory.append((rel, len(data)))


def sparse(root: Path, rel: str, size: int, inventory):
    p = root / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    with open(p, "wb") as f:
        f.truncate(size)
    inventory.append((rel, size))


def build(root: Path):
    rng = random.Random(SEED)
    if root.exists() and any(root.iterdir()):
        sys.exit(f"{root} is not empty")
    root.mkdir(parents=True, exist_ok=True)
    inv: list = []

    write(root, ".gitignore", "data/\n*.log\n", inv)
    write(root, "README.md", "# big-tree\n\nSynthetic scale fixture.\n", inv)
    # .git: version-control metadata, pruned and never counted.
    for rel in [".git/HEAD", ".git/config", ".git/objects/ab/cdef"]:
        write(root, rel, "ref: refs/heads/main\n", inv)

    # ~40,000 source files in nested directories.
    for d in range(400):
        depth = rng.randint(1, 5)
        parts = [f"m{rng.randint(0, 40)}" for _ in range(depth)]
        base = "src/" + "/".join(parts) + f"/d{d}"
        for f in range(100):
            ext = rng.choice(EXTS)
            write(root, f"{base}/f{f}.{ext}", text(rng, rng.randint(150, 3500), ext), inv)

    # Ambiguous directory names hold real source (N-01).
    for name in AMBIGUOUS:
        for f in range(200):
            write(root, f"lib/{name}/f{f}.py", text(rng, rng.randint(150, 2000), "py"), inv)

    # Size cap edges.
    write(root, "big/under_cap.txt", text(rng, CAP - 4096, "txt")[: CAP - 4096], inv)
    write(root, "big/over_cap.txt", text(rng, CAP + 4096, "txt")[: CAP + 4096], inv)

    # A Rust crate whose target/ is build output (pruned by marker).
    write(root, "crate/Cargo.toml", "[package]\nname = \"c\"\nversion = \"0.1.0\"\n", inv)
    write(root, "crate/src/lib.rs", "pub fn c() {}\n", inv)
    for f in range(300):
        write(root, f"crate/target/debug/deps/lib{f}.rlib.d", text(rng, 800, "txt"), inv)

    # ~200 MB of dependencies.
    for pkg in range(400):
        for f in range(50):
            write(root, f"node_modules/pkg{pkg}/lib/f{f}.js", text(rng, 10_000, "js")[:10_000], inv)

    # Gitignored data/ and logs.
    for f in range(3000):
        write(root, f"data/raw/r{f}.csv", text(rng, 1500, "txt"), inv)
    for f in range(20):
        write(root, f"logs/run{f}.log", text(rng, 500, "txt"), inv)

    # Sparse binary blobs: 30 × 100 MB with a binary extension, 4 × 50 MB sniffed.
    for i in range(30):
        sparse(root, f"assets/blob{i:02d}.bin", 100 * 1024 * 1024, inv)
    for i in range(4):
        sparse(root, f"assets/raw{i}.dat", 50 * 1024 * 1024, inv)

    # Symlinks: in-root file, to a directory, escaping, broken.
    links = root / "links"
    links.mkdir()
    outside = root.parent / (root.name + "-outside.txt")
    outside.write_text("outside the root\n")
    os.symlink("../README.md", links / "readme_link.md")
    os.symlink("../lib", links / "lib_dir")
    os.symlink(str(outside), links / "escape.txt")
    os.symlink("missing.rs", links / "broken.rs")
    for rel in ["links/readme_link.md", "links/lib_dir", "links/escape.txt", "links/broken.rs"]:
        inv.append((rel, 0))

    inventory = root.parent / (root.name + ".inventory.tsv")
    with open(inventory, "w") as f:
        for rel, size in sorted(inv):
            f.write(f"{rel}\t{size}\n")
    apparent = sum(s for _, s in inv)
    print(f"files={len(inv)} apparent_bytes={apparent} ({apparent / 2**30:.2f} GiB) inventory={inventory}")


def rule_matches(pattern: str, rel: str) -> bool:
    """The two rule shapes this fixture uses: `dir/` and `*.ext`."""
    parts = rel.split("/")
    if pattern.endswith("/"):
        return pattern[:-1] in parts[:-1]
    if pattern.startswith("*."):
        return rel.endswith(pattern[1:])
    return False


def verify(root: Path, out_json: Path):
    inventory = root.parent / (root.name + ".inventory.tsv")
    inv = [line.split("\t")[0] for line in inventory.read_text().splitlines()]
    doc = json.loads(out_json.read_text())
    merged = {f["path"] for f in doc["files"]}
    skipped = {s["path"]: s for s in doc["skipped"]}
    pruned = [s["path"] for s in doc["skipped"] if s["kind"] == "pruned_dir"]
    rules = [s["path"].split(": ", 1)[1] for s in doc["skipped"] if s["kind"] == "ignored_by_rule"]
    classes = {"merged": 0, "skipped": 0, "pruned_dir": 0, "ignored": 0}
    lost = []
    for rel in inv:
        if rel in merged:
            classes["merged"] += 1
        elif rel in skipped or rel + "/" in skipped:
            classes["skipped"] += 1
        elif any(rel.startswith(p) for p in pruned):
            classes["pruned_dir"] += 1
        elif any(rule_matches(r, rel) for r in rules):
            classes["ignored"] += 1
        else:
            lost.append(rel)
    print(f"inventory={len(inv)} " + " ".join(f"{k}={v}" for k, v in classes.items()) + f" unaccounted={len(lost)}")
    for rel in lost[:20]:
        print(f"  UNACCOUNTED {rel}")
    sys.exit(1 if lost else 0)


if __name__ == "__main__":
    if len(sys.argv) >= 3 and sys.argv[1] == "build":
        build(Path(sys.argv[2]).resolve())
    elif len(sys.argv) >= 4 and sys.argv[1] == "verify":
        verify(Path(sys.argv[2]).resolve(), Path(sys.argv[3]))
    else:
        sys.exit(__doc__)
