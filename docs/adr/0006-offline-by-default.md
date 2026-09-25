# 0006 — Network off by default; no Python in the product

- **Status:** Accepted (2026-09-24, confirmed by the maintainer; plan §18 q7, q8)
- **Decision:**
  - No network access unless the user asks for it (remote clones, arXiv-source
    enrichment later and opt-in, profile updates opt-in).
  - No Python runtime in the product. Users who want Docling or similar plug it in
    through an external-converter hook with a JSON-lines contract.
- **Consequences:** The Python spot-test scripts are reference oracles only.
