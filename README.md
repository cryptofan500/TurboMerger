# TurboMerger v6

A high-performance codebase merger that transforms entire project directories into a single, LLM-ready markdown file. Built with Rust and Tauri 2.0 for Windows.

![License](https://img.shields.io/badge/license-MIT-blue.svg)
![Platform](https://img.shields.io/badge/platform-Windows-lightgrey.svg)
![Rust](https://img.shields.io/badge/rust-1.70%2B-orange.svg)

## Features

- **Parallel Processing** - Multi-core file scanning with jwalk and Rayon
- **Smart Binary Detection** - 7-layer NuclearSieve pipeline filters non-text files
- **Memory Efficient** - Streaming architecture handles codebases of any size
- **Secret Redaction** - Pattern-based detection of API keys, passwords, and credentials
- **Virtual Environment Auto-Skip** - Detects and skips Python venv directories (500%+ faster on Python projects)
- **Lock File Exclusion** - Skips package-lock.json, Cargo.lock, yarn.lock to reduce context bloat
- **Modern UI** - React frontend with progress tracking and cancel support

## Installation

### Option 1: Download Release

Download the latest installer from [Releases](https://github.com/yourusername/turbomerger/releases):
- `TurboMerger_x.x.x_x64-setup.exe` (NSIS installer)
- `TurboMerger_x.x.x_x64_en-US.msi` (MSI installer)

### Option 2: Build from Source

#### Prerequisites

| Requirement | Version | Installation |
|-------------|---------|--------------|
| Windows | 10 or newer | - |
| Node.js | 18+ | [nodejs.org](https://nodejs.org/) |
| Rust | 1.70+ | [rustup.rs](https://rustup.rs/) |
| Visual Studio Build Tools | 2019+ | [Visual Studio](https://visualstudio.microsoft.com/downloads/) with "Desktop development with C++" workload |

#### Build Steps

```powershell
# Clone the repository
git clone https://github.com/yourusername/turbomerger.git
cd turbomerger

# Install Node.js dependencies
npm install

# Build the application (creates installer)
npm run tauri build
```

The built executable will be at:
```
src-tauri\target\release\turbomerger.exe
```

Installers will be at:
```
src-tauri\target\release\bundle\nsis\TurboMerger_x.x.x_x64-setup.exe
src-tauri\target\release\bundle\msi\TurboMerger_x.x.x_x64_en-US.msi
```

## Usage

1. **Launch TurboMerger** - Run the executable or installed application
2. **Select Source Directory** - Click "Select Folder" to choose a codebase
3. **Configure Options**:
   - Include directory tree (generates visual file structure)
   - Include virtual environments (default: skip for speed)
4. **Select Output Location** - Choose where to save the merged markdown file
5. **Click Merge** - Progress bar shows scanning and merging status
6. **Open Result** - Click "Open File" or "Open Folder" to access output

## Architecture

```
turbomerger/
├── src/                    # React/TypeScript frontend
│   ├── App.tsx             # Main UI component
│   └── styles/             # CSS styling
├── src-tauri/              # Rust backend
│   ├── src/
│   │   ├── lib.rs          # Tauri entry point
│   │   ├── commands.rs     # IPC command handlers
│   │   ├── merger/         # Core merge logic with streaming I/O
│   │   ├── scanner/        # Directory walking (jwalk + PHF)
│   │   └── security/       # Secret detection and path validation
│   └── resources/
│       └── extensions.json # Text/binary extension definitions
└── scripts/
    └── build_msi.ps1       # Build automation script
```

## Binary Detection (NuclearSieve)

The 7-layer pipeline ensures only text files are included:

| Layer | Check | Purpose |
|-------|-------|---------|
| 1 | Filename | Skip system files (NTUSER.DAT, Thumbs.db) |
| 2 | Extension | PHF lookup for 100+ known extensions |
| 3 | File Size | Skip files > 50MB |
| 4 | Magic Bytes | Detect PNG, JPEG, PDF, EXE, archives |
| 5 | NULL Bytes | Binary indicator scan |
| 6 | content_inspector | Library-based detection |
| 7 | Shannon Entropy | Detect compressed/encrypted content |

## Security Features

- **Strict CSP** - Content-Security-Policy prevents XSS attacks
- **Symlink Protection** - Reparse points (junctions/symlinks) are never followed
- **System Path Blocking** - Windows, Program Files, AppData directories blocked
- **Sensitive File Exclusion** - SSH keys, credentials, and secret files excluded
- **Secret Redaction** - Regex patterns detect and redact API keys in output

### Detected Secret Patterns

- AWS Access Keys and Secret Keys
- GitHub Personal Access Tokens
- Stripe API Keys (live and test)
- Google API Keys
- Private Keys (RSA, OpenSSH, PGP)
- Database Connection Strings
- JWT Tokens
- And 20+ more patterns

## Performance

| Project Size | Files | Approximate Time |
|--------------|-------|------------------|
| Small | 1,000 | ~2 seconds |
| Medium | 10,000 | ~15 seconds |
| Large | 50,000 | ~1 minute |
| Enterprise | 100,000+ | ~3 minutes |

Performance achieved through:
- **jwalk** - Parallel directory traversal
- **Rayon** - Multi-core file processing
- **PHF** - Compile-time perfect hash for O(1) extension lookups
- **Memory-mapped I/O** - Large file handling without memory exhaustion

## Skipped Directories (55+)

TurboMerger automatically skips directories that bloat output without adding value:

- **Version Control**: .git, .svn, .hg
- **Dependencies**: node_modules, vendor, packages
- **Build Outputs**: target, dist, build, out
- **Python**: venv, .venv, __pycache__, .pytest_cache
- **IDE**: .idea, .vscode, .vs
- **Caches**: .cache, .next, .nuxt

## Development

```powershell
# Install dependencies
npm install

# Run in development mode (hot reload)
npm run tauri dev

# Type check TypeScript
npm run typecheck

# Lint code
npm run lint

# Build for production
npm run tauri build
```

## Configuration Files

| File | Purpose |
|------|---------|
| `package.json` | Node.js dependencies and scripts |
| `tauri.conf.json` | Tauri app configuration |
| `Cargo.toml` | Rust dependencies |
| `tsconfig.json` | TypeScript configuration |
| `vite.config.ts` | Vite bundler settings |

## Troubleshooting

### Build fails with "MSVC not found"

Install Visual Studio Build Tools with the "Desktop development with C++" workload.

### Rust compilation errors

Ensure Rust is up to date:
```powershell
rustup update
```

### WebView2 runtime missing

The installer includes WebView2 bootstrapper. For manual builds, download from [Microsoft](https://developer.microsoft.com/en-us/microsoft-edge/webview2/).

### Large codebase runs out of memory

TurboMerger uses streaming I/O, but extremely large files (>50MB) are skipped. Check logs for skipped files.

## Contributing

1. Fork the repository
2. Create a feature branch (`git checkout -b feature/amazing-feature`)
3. Commit your changes (`git commit -m 'Add amazing feature'`)
4. Push to the branch (`git push origin feature/amazing-feature`)
5. Open a Pull Request

## License

MIT License - See [LICENSE](LICENSE) file for details.

## Acknowledgments

- [Tauri](https://tauri.app/) - Desktop app framework
- [Rayon](https://github.com/rayon-rs/rayon) - Data parallelism library
- [jwalk](https://github.com/jessegrosjean/jwalk) - Parallel directory walking
- [content_inspector](https://crates.io/crates/content_inspector) - Binary detection
- [PHF](https://github.com/rust-phf/rust-phf) - Compile-time hash maps

---

**Made for developers who need to feed entire codebases to LLMs.**
