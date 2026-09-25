# 0001 — Record decisions as ADRs

- **Status:** Accepted (2026-09-24)
- **Context:** TurboMerger is maintained by one person with several AI assistants
  working in separate sessions. Decisions made in chat are lost between sessions,
  and the refactor plan (§18) asks for an ADR per decision.
- **Decision:** Every decision that constrains later work gets a numbered file in
  `docs/adr/`. Sessions read the index before changing an area it covers.
- **Consequences:** Reversing a decision means writing a new ADR that supersedes the
  old one, not editing history.
