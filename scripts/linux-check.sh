#!/bin/bash
set -u

failures=0

pass() { printf 'ok   %s\n' "$1"; }
fail() { printf 'FAIL %s\n' "$1"; failures=$((failures + 1)); }
note() { printf 'note %s\n' "$1"; }
check_command() {
  if command -v "$1" >/dev/null 2>&1; then
    pass "$2"
  else
    fail "$2 ($1 not found)"
  fi
}
check_apt_pkg() {
  if dpkg -s "$1" >/dev/null 2>&1; then
    pass "$2"
  else
    fail "$2 (apt package $1 not installed)"
  fi
}

printf 'TurboMerger Linux build prerequisite check\n\n'

if [ "$(uname -s)" = "Linux" ]; then
  pass "Linux"
else
  fail "Linux required"
fi

if command -v apt >/dev/null 2>&1; then
  pass "apt (Debian/Ubuntu/Mint family)"
  check_apt_pkg libwebkit2gtk-4.1-dev "WebKitGTK 4.1 dev headers"
  check_apt_pkg build-essential "build-essential (gcc/make)"
  check_apt_pkg curl "curl"
  check_apt_pkg wget "wget"
  check_apt_pkg file "file"
  check_apt_pkg libxdo-dev "libxdo-dev"
  check_apt_pkg libssl-dev "libssl-dev"
  check_apt_pkg libayatana-appindicator3-dev "libayatana-appindicator3-dev"
  check_apt_pkg librsvg2-dev "librsvg2-dev"
else
  fail "apt not found — this checker targets Debian/Ubuntu/Mint; see docs/LINUX.md for other distros"
fi

check_command git "Git"
check_command node "Node.js 22+"
check_command npm "npm"
check_command rustc "Rust compiler"
check_command cargo "Cargo"

if command -v node >/dev/null 2>&1; then
  node_major="$(node -p 'process.versions.node.split(".")[0]')"
  if [ "$node_major" -ge 22 ] 2>/dev/null; then
    pass "Node.js version $(node -v)"
  else
    fail "Node.js 22 or newer required (found $(node -v))"
  fi
fi

# Informational only: Tauri's bundler (>=2.6) no longer needs libfuse to BUILD
# an AppImage, and current AppImage runtimes fall back to extract-and-run when
# libfuse2 is absent at launch time — so a missing libfuse2 is a note, not a
# failure. The .deb install path (recommended for Debian/Ubuntu/Mint) never
# touches FUSE at all.
if ldconfig -p 2>/dev/null | grep -q 'libfuse\.so\.2'; then
  pass "libfuse2 present (AppImage double-click will mount natively)"
else
  note "libfuse2/libfuse2t64 not found — the AppImage still runs via its extract-and-run fallback; prefer the .deb, or install one of: sudo apt install libfuse2t64 || sudo apt install libfuse2"
fi

printf '\n'
if [ "$failures" -eq 0 ]; then
  printf 'Ready. Next: npm ci && npm run verify && npm run tauri:build:linux\n'
  exit 0
fi

printf '%s prerequisite(s) need attention. See docs/LINUX.md.\n' "$failures"
printf 'Install the missing apt packages with:\n'
printf '  sudo apt update && sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget file libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev\n'
exit 1
