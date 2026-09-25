# poison-applyback — apply-back must never plant control files (N-52, N-17)

`reply.md` is an LLM reply that mixes one legitimate change (`src/main.rs`) with
proposals that would plant code which runs without the user asking:

| Proposal | Why it is dangerous |
|---|---|
| `.git/hooks/pre-commit` (existing, executable) | runs on the next `git commit`; v7.7.0 kept the `rwx` mode |
| `.GIT/hooks/post-checkout` | same file on case-insensitive filesystems (macOS, Windows) |
| `.github/workflows/ci.yml` | runs in CI on push |
| `.vscode/tasks.json` with `runOn: folderOpen` | runs when the folder is opened in VS Code |
| `.claude/settings.json` hooks | runs when an agent session starts in the folder |
| `build.rs` | runs on `cargo build` (manifest class: needs explicit confirmation) |
| `docs/note.md` (a symlink to a file outside the root) | writes outside the root (A.8) |

`build.sh <scratch>` creates the target repo. The Rust test
`src-tauri/tests/applyback_policy.rs` builds the same tree in a temp dir and
asserts the fixed behaviour on every OS.
