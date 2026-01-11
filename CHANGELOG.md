# Changelog

All notable changes to TurboMerger will be documented in this file.

## [6.0.0] - 2026-01-10

### Added
- **Enterprise GUI** - Modern React interface with real-time progress bar, cancel button, and output file selection
- **Virtual Environment Auto-Skip** - Automatically detects and skips Python venv directories (500%+ faster on Python projects)
- **Lock File Exclusion** - Skips package-lock.json, Cargo.lock, yarn.lock, and other lock files that bloat LLM context
- **Security Hardening** - Path validation, symlink protection, and restricted filesystem access
- **Memory-Safe Streaming I/O** - Handles codebases of any size without memory exhaustion

### Improved
- **Performance** - Parallel directory walking with jwalk and multi-core merging with Rayon
- **Binary Detection** - 7-layer NuclearSieve pipeline ensures only text files are included
- **Skip Directories** - Expanded to 55+ directories including IDE folders, build outputs, and Windows system paths

### Security
- Strict Content-Security-Policy
- Symlinks are never followed (prevents directory traversal attacks)
- System paths blocked (Windows, Program Files, AppData)
- SSH keys and credential files excluded

### Technical
- Built with Tauri 2.0 and Rust
- React 18 + TypeScript frontend
- PHF compile-time perfect hash for extension lookups
- Streaming architecture for large file handling

## [5.1.0] - Previous Release

- Ultra-Efficient mode for 100k+ file datasets
- Performance optimizations for large-scale scanning
- IPC payload reduction from 30-100MB to ~500 bytes
