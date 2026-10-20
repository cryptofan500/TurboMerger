#!/usr/bin/env bash
# Build the current source tree and install it for this user as
# "TurboMerger Dev": the GUI and the CLI in ~/.local/opt/TurboMerger-dev, a
# launcher with a teal icon, and (once) a taskbar pin next to the packaged
# TurboMerger. Run it after every change the Dev icon should pick up.
#
# The packaged TurboMerger (/usr/bin/turbomerger) is never touched. The Dev
# app has its own identifier (com.turbomerger.dev), so its settings and its
# apply-back backups are separate from the packaged app's.
#
# Usage: bash scripts/dev-install.sh [--no-pin]      (no sudo)
# Env:   TM_DEV_DEST (install folder), TM_DEV_JOBS (cargo jobs, default 4)
set -euo pipefail

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
DEST=${TM_DEV_DEST:-$HOME/.local/opt/TurboMerger-dev}
APPS=$HOME/.local/share/applications
DESKTOP=$APPS/turbomerger-dev.desktop
STATE=$HOME/.local/share/com.turbomerger.dev/state
PIN=1
[ "${1:-}" = "--no-pin" ] && PIN=0

# Vite's bundler needs Node 20+ (node:util styleText); prefer nvm's Node 22.
NODE_BIN=$(ls -d "$HOME"/.nvm/versions/node/v22.*/bin 2>/dev/null | sort -V | tail -1 || true)
[ -n "$NODE_BIN" ] && export PATH="$NODE_BIN:$PATH"
NODE_MAJOR=$(node --version | sed 's/^v\([0-9]*\).*/\1/')
if [ "$NODE_MAJOR" -lt 20 ]; then
  echo "error: Node $(node --version) is too old for the frontend build; install Node 22 (nvm install 22)" >&2
  exit 1
fi

cd "$REPO"
COMMIT=$(git rev-parse --short HEAD)
DIRTY=$(git status --porcelain --untracked-files=no | grep -q . && echo "+uncommitted changes" || true)

# Same window as tauri.conf.json, with a title that says which build it is.
DEV_CONF=$(python3 - "$COMMIT" <<'EOF'
import json, sys
c = json.load(open("src-tauri/tauri.conf.json"))
w = c["app"]["windows"]
w[0]["title"] = f"TurboMerger Dev ({sys.argv[1]})"
print(json.dumps({"identifier": "com.turbomerger.dev", "app": {"windows": w}}))
EOF
)

# Low-memory release profile (fat LTO needs several GB at link time).
export CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
export CARGO_BUILD_JOBS=${TM_DEV_JOBS:-4}
npx tauri build --no-bundle --config "$DEV_CONF"
cargo build --release -p tm-cli

mkdir -p "$DEST" "$APPS" "$STATE"
for bin in turbomerger-gui turbomerger; do
  # Copy then rename: a running Dev app keeps its old file until it restarts.
  cp -f "target/release/$bin" "$DEST/.$bin.new"
  mv -f "$DEST/.$bin.new" "$DEST/$bin"
done
cat > "$DEST/BUILD_INFO.txt" <<EOF
TurboMerger Dev, built $(TZ=America/Toronto date '+%Y-%m-%d %H:%M %Z')
commit $COMMIT on $(git branch --show-current) $DIRTY
$(rustc --version 2>/dev/null) · node $(node --version)
GUI: $DEST/turbomerger-gui   CLI: $DEST/turbomerger
EOF

# The packaged icon on a teal background (anti-aliased edges kept).
if [ ! -f "$DEST/turbomerger-dev.png" ]; then
  python3 - "$REPO/src-tauri/icons/128x128@2x.png" "$DEST/turbomerger-dev.png" <<'EOF'
import sys
from PIL import Image
src, out = sys.argv[1], sys.argv[2]
im = Image.open(src).convert("RGBA")
blue, white, teal = (30, 144, 255), (255, 255, 255), (13, 148, 136)
px = im.load()
for y in range(im.height):
    for x in range(im.width):
        r, g, b, a = px[x, y]
        t = max(0.0, min(1.0, (r - blue[0]) / (white[0] - blue[0])))
        px[x, y] = tuple(round((1 - t) * c + t * 255) for c in teal) + (a,)
im.save(out)
EOF
fi

cat > "$DESKTOP" <<EOF
[Desktop Entry]
Type=Application
Name=TurboMerger Dev
Comment=Newest TurboMerger built from the source tree (the packaged TurboMerger is the old 2026-08-06 build)
Exec=env TURBOMERGER_STATE_DIR=$STATE $DEST/turbomerger-gui
Icon=$DEST/turbomerger-dev.png
StartupWMClass=turbomerger-gui
Categories=Development;
Terminal=false
EOF

# Pin once to the Cinnamon taskbar, right after the packaged TurboMerger.
if [ "$PIN" = 1 ]; then
  for f in "$HOME"/.config/cinnamon/spices/grouped-window-list@cinnamon.org/*.json; do
    [ -f "$f" ] || continue
    python3 - "$f" <<'EOF'
import json, sys
p = sys.argv[1]
d = json.load(open(p))
pins = d.get("pinned-apps", {}).get("value")
if isinstance(pins, list) and "turbomerger-dev.desktop" not in pins:
    at = pins.index("TurboMerger.desktop") + 1 if "TurboMerger.desktop" in pins else len(pins)
    pins.insert(at, "turbomerger-dev.desktop")
    json.dump(d, open(p, "w"), indent=4)
    print(f"pinned to the taskbar ({p})")
EOF
  done
fi

cat "$DEST/BUILD_INFO.txt"
