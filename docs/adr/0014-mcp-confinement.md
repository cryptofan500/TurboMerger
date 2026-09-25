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
  - **Done in Phase 2:** `apps/tm-mcp` runs on the official SDK, `rmcp` =3.4.1
    (current-thread tokio runtime). Negotiation is the SDK's: a known version is
    accepted, anything else gets 2025-11-25 (tested: `2099-01-01` is never echoed).
    Long calls send `notifications/progress` when the client passes a progress token;
    `notifications/cancelled` stops a pack through its `CancelToken` and nothing is
    written. The policy above is unchanged and tested against the real service over an
    in-memory transport.
- **Consequences:** existing MCP client configs must add `--root <DIR>` when the
  server starts in `/` or the home folder; tools report the fix in their error text.
