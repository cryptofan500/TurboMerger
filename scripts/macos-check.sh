#!/bin/bash
set -u

failures=0

pass() { printf 'ok   %s\n' "$1"; }
fail() { printf 'FAIL %s\n' "$1"; failures=$((failures + 1)); }
check_command() {
  if command -v "$1" >/dev/null 2>&1; then
    pass "$2"
  else
    fail "$2 ($1 not found)"
  fi
}

printf 'TurboMerger macOS build prerequisite check\n\n'

if [ "$(uname -s)" = "Darwin" ]; then
  pass "macOS"
else
  fail "macOS required"
fi

if [ "$(uname -m)" = "arm64" ]; then
  pass "Apple Silicon (arm64)"
else
  fail "Apple Silicon required for the M4 build path"
fi

if xcode-select -p >/dev/null 2>&1; then
  pass "Xcode Command Line Tools"
else
  fail "Xcode Command Line Tools (run: xcode-select --install)"
fi

check_command git "Git"
check_command node "Node.js 22+"
check_command npm "npm"
check_command rustc "Rust compiler"
check_command cargo "Cargo"
check_command xcrun "Apple build tools"

if command -v node >/dev/null 2>&1; then
  node_major="$(node -p 'process.versions.node.split(".")[0]')"
  node_arch="$(node -p 'process.arch')"
  if [ "$node_major" -ge 22 ] 2>/dev/null; then
    pass "Node.js version $(node -v)"
  else
    fail "Node.js 22 or newer required (found $(node -v))"
  fi
  if [ "$node_arch" = "arm64" ]; then
    pass "Node.js is native arm64"
  else
    fail "Node.js is $node_arch; install the native Apple Silicon build"
  fi
fi

printf '\n'
if [ "$failures" -eq 0 ]; then
  printf 'Ready. Next: npm ci && npm run verify && npm run tauri:build:mac\n'
  exit 0
fi

printf '%s prerequisite(s) need attention. See docs/MACOS.md.\n' "$failures"
exit 1
