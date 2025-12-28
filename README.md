# TurboMerger v4.0

High-performance code merger with secret scanning, optimized for feeding large codebases into LLM context windows.

## Features

- **Smart Scanning** - Intelligently filters code files, skips binaries and dependencies
- **Secret Detection** - Catches API keys, tokens, and credentials before they leak
- **PDF Extraction** - Extracts text from PDF documentation
- **Token Counting** - Accurate tiktoken-based token estimation for GPT models
- **Multithreaded** - Parallel file processing with cooperative cancellation
- **Config Profiles** - Save and load scan configurations
- **.gitignore Support** - Respects your project's .gitignore patterns
- **Dark/Light Themes** - Modern CustomTkinter interface
- **Windows Integration** - Right-click context menu support via installer

## Installation

### From Source (Development)

Requires Python 3.11+ and [uv](https://docs.astral.sh/uv/):

```bash
# Clone the repository
git clone https://github.com/cryptofan500/turbomerger.git
cd turbomerger

# Install dependencies
uv sync

# Run the application
uv run python -m turbomerger
```

### Windows Installer

Download the latest `TurboMerger-X.X.X-Setup.exe` from [Releases](https://github.com/cryptofan500/turbomerger/releases).

### Standalone Executable

Download `turbomerger.exe` from [Releases](https://github.com/cryptofan500/turbomerger/releases) - no installation required.

## Usage

### GUI Mode

```bash
# Using uv (development)
uv run python -m turbomerger

# Using the executable
turbomerger.exe
```

### Scan Modes

| Mode | Description |
|------|-------------|
| **Smart Scan** | Code files only (.py, .js, .ts, .go, .rs, etc.) |
| **Complete Scan** | Code + documentation (.md, .txt, .rst, etc.) |

### Configuration Options

- **Max File Size** - Skip files larger than this (default: 2 MB)
- **Max Characters** - Truncate files exceeding this limit (default: 100,000)
- **Respect .gitignore** - Honor project's .gitignore patterns (default: on)

### Output Format

TurboMerger creates a structured markdown file with:

```markdown
# Project: my-project
Generated: 2024-12-27 10:30:00

## Table of Contents
- [src/main.py](#src-main-py)
- [src/utils.py](#src-utils-py)

---

## src/main.py
```python
# File contents here
```

## Security Features

TurboMerger scans for:

- AWS Access Keys & Secret Keys
- OpenAI API Keys (including new sk-proj- format)
- Stripe Live/Test Keys
- GitHub Personal Access Tokens
- Google API Keys
- SSH Private Keys
- Generic high-entropy secrets

Files flagged with secrets are highlighted in the Security tab and excluded from output.

## Building from Source

### Windows Executable (Nuitka)

```bash
# One-file build (smaller, slower startup)
build_scripts\build_exe.bat

# Folder build (larger, faster startup)
build_scripts\build_standalone.bat
```

### Windows Installer (Inno Setup)

1. Build the executable first
2. Install [Inno Setup 6.6+](https://jrsoftware.org/isinfo.php)
3. Compile `build_scripts\setup.iss`

## Development

### Running Tests

```bash
uv run pytest
```

### Code Quality

```bash
# Linting
uv run ruff check src tests

# Formatting
uv run ruff format src tests

# Type checking
uv run mypy src
```

## Requirements

- Python 3.11+
- Windows 10 version 1903+ (for installer)

### Dependencies

- customtkinter - Modern Tkinter widgets
- pypdf - PDF text extraction
- tiktoken - OpenAI token counting

## License

MIT License - See [LICENSE](LICENSE) for details.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## Changelog

### v4.0.0

- Complete rewrite with CustomTkinter GUI
- Added secret scanning with comprehensive patterns
- Added tiktoken token counting
- Added .gitignore support
- Multithreaded file processing
- Config profile save/load
- Windows installer with context menu integration
