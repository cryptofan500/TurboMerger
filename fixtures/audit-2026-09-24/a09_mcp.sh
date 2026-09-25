#!/usr/bin/env bash
# A.9 — v7.7.0: echoes protocolVersion 2099-01-01; pack_directory writes wherever
# "output" points (.bashrc_probe). Fixed: a supported version is negotiated; outputs
# land only in the managed outputs directory; paths outside --root are refused.
. "$(dirname "$0")/common.sh"
[[ -d "$SP/detrepo" ]] || bash "$here/a05_determinism.sh" >/dev/null
mkdir -p "$SP/mcp_any"
printf '%s\n' \
 '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2099-01-01","capabilities":{},"clientInfo":{"name":"audit","version":"0"}}}' \
 "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"pack_directory\",\"arguments\":{\"path\":\"$SP/detrepo\",\"output\":\"$SP/mcp_any/.bashrc_probe\"}}}" \
 | "$TM" mcp
ls -la "$SP/mcp_any"
