# Linux x64 runbook

TurboMerger targets `x86_64-unknown-linux-gnu` (Debian/Ubuntu/Mint and compatible
distros) using the system WebKitGTK webview. Release builds produce a `.deb`
package and a portable `.AppImage`. Neither is signed — there is no Linux
equivalent of macOS notarization or a Windows code-signing certificate here.

## Recommended: install the release `.deb`

For Debian, Ubuntu, and Linux Mint (or any apt-based distro), the `.deb` is the
lowest-friction path: it installs a desktop entry and icon, and declares its own
library dependencies so apt/dpkg resolves them for you.

1. Open the repository's **Releases** page.
2. Download `TurboMerger_<version>_amd64.deb` and `SHA256SUMS.txt` from the same
   release.
3. Verify the file before installing it:

   ```bash
   cd ~/Downloads
   sha256sum TurboMerger_*.deb
   ```

   Compare the result character-for-character with the `.deb` line in
   `SHA256SUMS.txt`.

4. Install it:

   ```bash
   sudo apt install ./TurboMerger_*.deb
   ```

   (`apt install ./file.deb` — not `dpkg -i` — so apt also pulls in any missing
   runtime libraries.)

5. Launch **TurboMerger** from your application menu, or run `turbomerger`.

## Alternative: the portable `.AppImage`

Use this on non-Debian distros (Fedora, Arch, openSUSE, …) or when you don't
want to install a package.

1. Download `TurboMerger_<version>_amd64.AppImage` and `SHA256SUMS.txt`.
2. Verify it the same way as above (`sha256sum TurboMerger_*.AppImage`).
3. Make it executable and run it:

   ```bash
   chmod +x TurboMerger_*.AppImage
   ./TurboMerger_*.AppImage
   ```

If it fails to launch with a `libfuse.so.2` error, either install FUSE2
(`sudo apt install libfuse2t64` on Ubuntu 24.04/Mint 22.x and newer, or
`sudo apt install libfuse2` on older releases) or run it with
`./TurboMerger_*.AppImage --appimage-extract-and-run`, which needs no FUSE at all.

## Build from source

### 1. Install prerequisites

```bash
sudo apt update
sudo apt install libwebkit2gtk-4.1-dev \
  build-essential \
  curl \
  wget \
  file \
  libxdo-dev \
  libssl-dev \
  libayatana-appindicator3-dev \
  librsvg2-dev
```

Install Node.js 22+ and Git (via your distro's package manager, `nvm`, or
nodesource), and stable Rust through [rustup](https://rustup.rs):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Then check the machine locally:

```bash
bash scripts/linux-check.sh
```

The checker writes no report and collects no system or account data.

### 2. Install and verify

From a clean clone:

```bash
npm ci
npm run verify
```

`npm ci` uses the committed lock file. `npm run verify` checks version
agreement, ESLint, both TypeScript configurations, the production frontend,
rustfmt, Clippy, and all Rust unit/integration tests.

### 3. Run or package

Development mode:

```bash
npm run tauri:dev
```

Release build:

```bash
npm run tauri:build:linux
```

Outputs:

```text
target/release/turbomerger-gui        # the desktop app
target/release/turbomerger            # the command line (also inside the .deb)
target/release/bundle/deb/*.deb
target/release/bundle/appimage/*.AppImage
```

### 4. Verify the bundle

```bash
dpkg --info target/release/bundle/deb/*.deb
dpkg-deb -c target/release/bundle/deb/*.deb | grep usr/bin/   # turbomerger + turbomerger-gui
file target/release/bundle/appimage/*.AppImage
./target/release/turbomerger --help >/dev/null && echo "binary runs"
```

## Smoke test

Use throwaway data for apply-back testing.

- Launch the GUI and select a small project under your home directory.
- Merge with redaction and gitignore handling enabled.
- Confirm the result opens and **Show in Folder** reveals the containing folder
  in your file manager (Nemo on Linux Mint Cinnamon, Nautilus on GNOME, …).
  Unlike Windows Explorer and macOS Finder, the Linux path opens the parent
  folder rather than pre-selecting the file — this is a `xdg-open` limitation,
  not a bug.
- Confirm `node_modules`, `target`, `.git`, and `.turbomerger` are skipped.
- Pack a small public remote repository to exercise the shallow-clone path.
- Turn on watch mode, edit a source file, and confirm exactly one refresh occurs.
- Preview an apply-back response, accept it, and restore the backup.

Windows and macOS CI validate the shared Rust/TypeScript logic, but only a real
Linux run confirms WebKitGTK rendering, the `.deb`/AppImage bundles, and file
manager integration.

## Troubleshooting

### `error: failed to run custom build command for 'webkit2gtk-sys'` or similar dev-header errors

Run `bash scripts/linux-check.sh` to see exactly which apt package is missing,
then install it with the command the script prints.

### AppImage: `dlopen(): error loading libfuse.so.2`

Ubuntu 24.04 and Linux Mint 22.x renamed the package to `libfuse2t64`:

```bash
sudo apt install libfuse2t64 || sudo apt install libfuse2
```

Or skip FUSE entirely: `./TurboMerger_*.AppImage --appimage-extract-and-run`.
The `.deb` install path never needs FUSE.

### `version 'GLIBC_2.3x' not found` when running a downloaded build

The release binary is built on Ubuntu 24.04 (glibc 2.39). Running it on a
distro with an older glibc than the build host is not supported — build from
source on that machine instead (see above), or use a distro at least as new
as Ubuntu 22.04/Debian 12.

### Remote packing says `git` is unavailable

```bash
sudo apt install git
```

### A dependency reports the wrong architecture

Do not copy `node_modules` or `target` from Windows or macOS.

```bash
rm -rf node_modules target
npm ci
```
