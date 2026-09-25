#!/usr/bin/env bash
# A.5 — v7.7.0: 12 runs give 2 distinct outputs; half leak the secret's tail
# ("[REDACTED]W8nB3c"). Fixed: 1 distinct output, no fragment.
. "$(dirname "$0")/common.sh"
rm -rf "$SP/detrepo"; mkdir -p "$SP/detrepo"
printf 'API_TOKEN=Qx7Zk2Mv9Rt4Lp\nAPI_TOKEN_V2=Qx7Zk2Mv9Rt4LpW8nB3c\n' > "$SP/detrepo/.env"
printf '# Notes\nThe old value was Qx7Zk2Mv9Rt4Lp and the rotated value is Qx7Zk2Mv9Rt4LpW8nB3c today.\n' > "$SP/detrepo/NOTES.md"
printf 'fn main() {}\n' > "$SP/detrepo/main.rs"
for i in $(seq 1 12); do "$TM" merge "$SP/detrepo" "$SP/det_$i.md" -q || true; grep -h "rotated value" "$SP/det_$i.md"; done | sort | uniq -c
sha256sum "$SP"/det_*.md | cut -d' ' -f1 | sort -u | wc -l
