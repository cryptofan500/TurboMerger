# 0010 — Apply-back: control-path policy, handle-relative writes, trusted restore

- **Status:** Accepted (2026-09-24) — implemented on `refactor/phase-0` (plan §15, §18 q14; findings N-52, N-17, N-15)
- **Context:** An LLM reply is untrusted input. v7.7.0 applied a poisoned reply
  (fixture `fixtures/poison-applyback`) in full, exit 0: it overwrote an executable
  `.git/hooks/pre-commit` (mode kept `rwx`), created a CI workflow, a VS Code
  folder-open task and `build.rs`, and wrote through a planted symlink to a file
  outside the root. It also re-encoded a Windows-1252 file as UTF-8, corrupting
  lines the diff never touched.
- **Decision:**
  1. **Path classes** (`applyback/policy.rs`), matched on normalized components
     (ASCII-lowercased, zero-width/bidi characters removed, trailing dots/spaces
     trimmed, NTFS `git~1` recognized):
     - *forbidden* — anything inside a `.git` directory; no override exists;
     - *control* — git attributes/modules, hook managers, CI pipelines, editor and
       coding-agent settings, env loaders, package-manager/toolchain configs that run
       code: held until `--allow-control GLOB` or a per-file GUI confirmation;
     - *manifest* — build/dependency manifests: held until `--allow-manifest GLOB`
       or a per-file confirmation;
     - *code* — everything else.
  2. **Executable bits:** files executable today are refused unless `--allow-exec`
     (the mode is then kept); created files never get an executable bit.
  3. **Handle-relative I/O** (`applyback/safe_fs.rs`, cap-std >= 4.0.3 for
     GHSA-hp8f-xmx4-4qrg): every component is opened with `O_NOFOLLOW`
     semantics from a root handle; a symlink or junction anywhere on the path is
     refused, never followed; replacements are temp file + rename in the same
     directory handle (hard links are detached, not written through); new files
     use `O_EXCL`; after opening, the OS-reported path (`/proc/self/fd`,
     `F_GETPATH`, `GetFinalPathNameByHandleW`) must lie under the root and match
     the requested name (catches case variants and 8.3 names).
  4. **Encoding round-trip** (`applyback/encoding.rs`): a target is editable only
     when its detected encoding (BOM → UTF-8 → chardetng) re-encodes to identical
     bytes; new content is written in the file's encoding and BOM, and characters
     it cannot represent are refused.
  5. **Other refusals:** deletions (shown, never executed), binary targets,
     proposals that differ only by case (from each other or from an existing
     file), and replies that would write `[REDACTED]` placeholders over real values.
  6. **Trusted restore:** every apply records `(root, backup, manifest hash)` in
     `<data dir>/com.turbomerger.app/apply-backups.jsonl` (or
     `$TURBOMERGER_STATE_DIR`). `--restore` only honours recorded backups, so a
     `.turbomerger/backups/` tree shipped inside a cloned repository cannot plant
     files; entries edited since the apply are skipped and reported.
- **Consequences:** CLI `apply` exits 3 when proposals were held or refused. The GUI
  shows class badges and a per-file confirmation for control/manifest files.
  Restoring requires the local record; users who wipe their data dir restore by hand
  from the backup folder the error names.
