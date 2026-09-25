#!/usr/bin/env bash
# A.7 — v7.7.0: /run/user/$UID -> "Access denied: system path"; full-width and NFD
# folder names -> "No such file or directory". Fixed: both merge, exit 0.
. "$(dirname "$0")/common.sh"
mkdir -p "/run/user/$(id -u)/tm_probe" && printf 'fn a(){}\n' > "/run/user/$(id -u)/tm_probe/a.rs"
set +e
"$TM" merge "/run/user/$(id -u)/tm_probe" "$SP/run_out.md" -q; echo "exit=$?"
python3 -c "import os,unicodedata as u;[os.makedirs(os.path.join('$SP/nfkc',n),exist_ok=True) or open(os.path.join('$SP/nfkc',n,'x.rs'),'w').write('fn x(){}\n') for n in ['ＡＢＣ_fullwidth',u.normalize('NFD','café_nfd')]]"
for d in "$SP"/nfkc/*; do "$TM" merge "$d" "$SP/nfkc_out.md" -q; echo "exit=$?"; done
rm -rf "/run/user/$(id -u)/tm_probe"
