# 0014 — MCP: root confinement, managed outputs, version negotiation; SDK in Phase 2

- **Status:** Accepted (2026-09-24) — implemented (findings N-18, N-19)
- **Context:** An MCP client is an agent that may be prompt-injected. v7.7.0's
  `pack_directory` packed any readable folder and wrote wherever the client said
  (`…/.bashrc_probe`), `read_output` accepted any file whose name contained
  `_merged.`, and `initialize` echoed any protocol version (`2099-01-01`).
- **Decision:**
  - `turbomerger mcp --root DIR …`: only folders under these roots can be packed or
    mapped. Default: the working directory, but never `/` or the home folder.
  - Outputs always go to a managed directory (`--output-dir`, default
    `<data dir>/com.turbomerger.app/mcp-outputs`); the client's `output` is a file-name
    hint only. `read_output`/`grep_output` serve files inside that directory only.
  - Remote repositories need `--allow-remote`.
  - `initialize` answers with the client's version when supported
    (2025-11-25, 2025-06-18, 2025-03-26, 2024-11-05), otherwise with 2025-11-25.
  - **Deferred:** migrating to the official `rmcp` SDK (3.4.1) moves to Phase 2,
    together with the `tm-mcp` crate. rmcp 3.4.1 knows the 2026-07-28 spec but its own
    default (`ProtocolVersion::LATEST`) is still 2025-11-25, and the migration brings an
    async runtime into the MCP path; the Phase-1 gate (repro A.9) is met without it.
- **Consequences:** existing MCP client configs must add `--root <DIR>` when the
  server starts in `/` or the home folder; tools report the fix in their error text.
