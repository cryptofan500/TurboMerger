# Release procedure

The release workflow builds Windows x64, macOS Apple Silicon, and Linux x64
artifacts, verifies the ARM app signature/architecture and DMG, combines all
platforms in one publisher job, writes one `SHA256SUMS.txt`, and creates a draft
GitHub Release.

## Before tagging

1. Update `CHANGELOG.md`.
2. Keep these versions identical:
   - `package.json`
   - `Cargo.toml` (`[workspace.package].version`; every crate inherits it)
   - `src-tauri/tauri.conf.json`
3. From a clean dependency install, run:

   ```bash
   npm ci
   npm run verify
   ```

4. Push a branch and require the Windows, ARM64 macOS, and Linux x64 CI jobs to pass.

## Create the draft

For version `7.7.0`, create and push the matching tag:

```bash
git tag -a v7.7.0 -m "TurboMerger 7.7.0"
git push origin v7.7.0
```

The workflow rejects malformed tags and any tag that disagrees with the three
version files. A manual workflow run must name an existing `vMAJOR.MINOR.PATCH` tag.

The friend release uses Tauri ad-hoc signing (`signingIdentity: "-"`); it requires
no signing secret. Do not describe it as notarized.

## Before publishing the draft

1. Download the draft assets and `SHA256SUMS.txt`.
2. Verify every checksum.
3. On a real Apple Silicon Mac, run the smoke test in `docs/MACOS.md`.
4. On Windows, install the NSIS build and run a local merge/open/apply smoke test.
5. On Linux (Debian/Ubuntu/Mint), install the `.deb` and run the smoke test in
   `docs/LINUX.md`; spot-check the `.AppImage` launches too.
6. Confirm the release notes say **Apple Silicon**, **ad-hoc signed**, and
   **not notarized**, and that the Linux artifacts are described as unsigned
   (there is no Linux equivalent of notarization here).
7. Publish the draft only after all three platform checks pass.

Developer ID signing and Apple notarization are a separate distribution path. If
added later, follow current Tauri and Apple documentation and store credentials only
as protected GitHub secrets; never commit certificates, passwords, or API keys.
