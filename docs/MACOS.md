# macOS Apple Silicon runbook

TurboMerger targets native Apple Silicon (`aarch64-apple-darwin`), including
M1, M2, M3, and M4 Macs. It uses the system WKWebView and creates an ad-hoc-signed
`.app` and `.dmg` for trusted personal testing.

Ad-hoc signing is not Apple notarization. It makes an ARM application structurally
signed, but a downloaded build can still require the owner's explicit Gatekeeper
approval.

## Recommended: install the release DMG

1. Open the repository's **Releases** page.
2. Download the Apple Silicon `.dmg` and `SHA256SUMS.txt` from the same release.
3. Verify the file before opening it:

   ```bash
   cd ~/Downloads
   shasum -a 256 TurboMerger*.dmg
   ```

   Compare the result character-for-character with the DMG line in
   `SHA256SUMS.txt`.

4. Open the DMG and drag TurboMerger to Applications.
5. Open TurboMerger from Applications.

If macOS blocks an unnotarized download that you trust, attempt to open it once,
then open **System Settings → Privacy & Security**, scroll to **Security**, and
choose **Open Anyway** for TurboMerger. Authenticate when macOS asks. Do not bypass
Gatekeeper for software whose source or checksum you do not trust.

## Build from source on an M4

### 1. Install prerequisites

For a desktop-only Tauri build, Xcode Command Line Tools are sufficient:

```bash
xcode-select --install
```

Install native Apple Silicon builds of:

- Node.js 22 or 24 and npm 10+;
- Git; and
- stable Rust through [rustup](https://rustup.rs).

Homebrew is optional, but if already installed it can provide Node and Git:

```bash
brew install node git
```

Restart Terminal after installing tools, then check the machine locally:

```bash
bash scripts/macos-check.sh
```

The checker writes no report and collects no MDM, account, or administrator data.

### 2. Install and verify

From a clean clone:

```bash
rustup target add aarch64-apple-darwin
npm ci
npm run verify
```

`npm ci` uses the committed lock file. `npm run verify` checks version agreement,
ESLint, both TypeScript configurations, the production frontend, rustfmt, Clippy,
and all Rust unit/integration tests.

### 3. Run or package

Development mode:

```bash
npm run tauri:dev
```

Native Apple Silicon release:

```bash
npm run tauri:build:mac
```

Outputs:

```text
src-tauri/target/aarch64-apple-darwin/release/turbomerger
src-tauri/target/aarch64-apple-darwin/release/bundle/macos/TurboMerger.app
src-tauri/target/aarch64-apple-darwin/release/bundle/dmg/*.dmg
```

The repository commits its complete icon set, including `icon.icns`; do not run
`tauri icon` during ordinary builds.

Tauri may warn that the existing bundle identifier `com.turbomerger.app` ends in
`.app`. It is retained for upgrade/settings compatibility with earlier TurboMerger
builds; the warning does not prevent this build.

### 4. Verify the bundle

```bash
APP="src-tauri/target/aarch64-apple-darwin/release/bundle/macos/TurboMerger.app"
BIN="$(find "$APP/Contents/MacOS" -maxdepth 1 -type f -print -quit)"

lipo -archs "$BIN"                         # must include arm64
codesign --verify --deep --strict "$APP"   # verifies the ad-hoc signature
hdiutil verify src-tauri/target/aarch64-apple-darwin/release/bundle/dmg/*.dmg
open "$APP"
```

## M4 smoke test

Use throwaway data for apply-back testing.

- Launch the GUI and select a small project under your home directory.
- Merge with redaction and gitignore handling enabled.
- Confirm the result opens and **Show in Folder** reveals it in Finder.
- Confirm `node_modules`, `target`, `.git`, and `.turbomerger` are skipped.
- Pack a small public remote repository to exercise macOS temporary directories.
- Turn on watch mode, edit a source file, and confirm exactly one refresh occurs.
- While watching, confirm folder drag/drop cannot silently switch the watched root.
- Preview an apply-back response, accept it, and restore the backup.
- If relevant, test Desktop/Documents/Downloads access prompts on the friend's Mac.

Windows can validate shared Rust/TypeScript logic, but only a real macOS runner can
certify WKWebView, Finder reveal, FSEvents, Gatekeeper, code signing, DMG integrity,
and macOS privacy prompts. The included GitHub workflow performs the native ARM64
build; the friend smoke test remains the final release gate.

## Troubleshooting

### `xcode-select` or C compiler errors

Run `xcode-select --install`, let installation finish, then open a new Terminal.

### A dependency reports the wrong architecture

Do not copy `node_modules` or `src-tauri/target` from Windows or Intel macOS.

```bash
rm -rf node_modules src-tauri/target
npm ci
```

`node -p process.arch` and `uname -m` should both print `arm64`.

### Remote packing says `git` is unavailable

Install Git or make sure the Xcode Command Line Tools Git is on `PATH`.

### Gatekeeper blocks the downloaded app

Verify the release checksum first. Then follow the **Privacy & Security → Open
Anyway** flow above. A public, warning-free release requires an Apple Developer ID
and notarization, which this friend build intentionally does not claim.

### A company-managed Mac refuses the app

Do not attempt to defeat organization policy. Ask the device administrator to
approve the app, or use a properly Developer-ID-signed and notarized build.
