"""File type detection and filtering utilities."""

from pathlib import Path
from typing import Set

# Files to ALWAYS skip (lock files, system files) - BUG #1 FIX
SKIP_FILES: Set[str] = {
    # Package manager lock files (waste tokens, no value)
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "poetry.lock",
    "Pipfile.lock",
    "composer.lock",
    "Cargo.lock",
    "uv.lock",
    "bun.lockb",
    "gemfile.lock",
    # System files
    ".DS_Store",
    "Thumbs.db",
    "desktop.ini",
    "ehthumbs.db",
    # Generated files
    ".gitattributes",
}

# Files to KEEP in Smart mode even though they're .md - BUG #3 FIX
KEEP_IN_SMART_MODE: Set[str] = {
    "README.MD",
    "README.TXT",
    "README",
    "CONTRIBUTING.MD",
    "CHANGELOG.MD",
    "CHANGES.MD",
    "ARCHITECTURE.MD",
    "SECURITY.MD",
    "AUTHORS.MD",
    "LICENSE.MD",
    "CODE_OF_CONDUCT.MD",
}

# Extensions for code files (Smart Scan mode)
CODE_EXTENSIONS: Set[str] = {
    # Python
    ".py", ".pyi", ".pyx", ".pxd",
    # JavaScript/TypeScript
    ".js", ".jsx", ".ts", ".tsx", ".mjs", ".cjs",
    # Web
    ".html", ".htm", ".css", ".scss", ".sass", ".less", ".vue", ".svelte",
    # Systems
    ".c", ".h", ".cpp", ".hpp", ".cc", ".cxx", ".hxx",
    # JVM
    ".java", ".kt", ".kts", ".scala", ".groovy",
    # .NET
    ".cs", ".fs", ".vb",
    # Go/Rust
    ".go", ".rs",
    # Ruby/PHP
    ".rb", ".php",
    # Shell/Scripts
    ".sh", ".bash", ".zsh", ".fish", ".ps1", ".psm1", ".bat", ".cmd",
    # Config (code-like)
    ".json", ".yaml", ".yml", ".toml", ".ini", ".cfg",
    # Data/Query
    ".sql", ".graphql", ".gql",
    # Mobile
    ".swift", ".m", ".mm",
    # Other
    ".r", ".R", ".jl", ".lua", ".pl", ".pm", ".dart", ".elm", ".ex", ".exs",
    ".clj", ".cljs", ".edn", ".hs", ".ml", ".mli", ".f90", ".f95",
}

# Extensions for document files (Complete Scan mode adds these)
DOC_EXTENSIONS: Set[str] = {
    ".md", ".markdown", ".rst", ".txt", ".adoc", ".asciidoc",
    ".tex", ".rtf", ".org",
}

# Extensions for PDF files
PDF_EXTENSIONS: Set[str] = {".pdf"}

# Extensions to ALWAYS skip (binary/media)
BINARY_EXTENSIONS: Set[str] = {
    # Images
    ".png", ".jpg", ".jpeg", ".gif", ".bmp", ".ico", ".svg", ".webp", ".tiff", ".tif",
    # Audio
    ".mp3", ".wav", ".ogg", ".flac", ".aac", ".m4a",
    # Video
    ".mp4", ".avi", ".mkv", ".mov", ".wmv", ".webm",
    # Archives
    ".zip", ".tar", ".gz", ".bz2", ".xz", ".7z", ".rar",
    # Compiled
    ".exe", ".dll", ".so", ".dylib", ".o", ".obj", ".pyc", ".pyo", ".class",
    # Fonts
    ".ttf", ".otf", ".woff", ".woff2", ".eot",
    # Office (binary)
    ".doc", ".xls", ".ppt", ".docx", ".xlsx", ".pptx",
    # Database
    ".db", ".sqlite", ".sqlite3", ".mdb",
    # Other binary
    ".bin", ".dat", ".pak", ".wasm",
}

# Directories to skip
SKIP_DIRECTORIES: Set[str] = {
    # Version control
    ".git", ".svn", ".hg",
    # Dependencies
    "node_modules", "vendor", "bower_components",
    "__pycache__", ".pytest_cache", ".mypy_cache", ".ruff_cache",
    "venv", ".venv", "env", ".env",
    # Build output
    "build", "dist", "out", "target", "bin", "obj",
    ".next", ".nuxt", ".output",
    # IDE
    ".idea", ".vscode",
    # Misc
    "coverage", "htmlcov", ".tox", ".nox",
    ".eggs", "*.egg-info",
}

# Maximum file size for binary check (read first N bytes)
BINARY_CHECK_SIZE = 8192


def is_binary(file_path: Path) -> bool:
    """
    Check if a file is binary by looking for null bytes.

    Args:
        file_path: Path to the file to check.

    Returns:
        True if the file appears to be binary, False otherwise.
    """
    try:
        with open(file_path, "rb") as f:
            chunk = f.read(BINARY_CHECK_SIZE)
            # Null bytes indicate binary
            if b"\x00" in chunk:
                return True
            # Try to decode as UTF-8
            try:
                chunk.decode("utf-8")
                return False
            except UnicodeDecodeError:
                # Try common encodings
                for encoding in ("latin-1", "cp1252"):
                    try:
                        chunk.decode(encoding)
                        return False
                    except UnicodeDecodeError:
                        continue
                return True
    except (OSError, IOError):
        return True


def is_code_file(file_path: Path) -> bool:
    """Check if a file is a code file based on extension."""
    return file_path.suffix.lower() in CODE_EXTENSIONS


def is_doc_file(file_path: Path) -> bool:
    """Check if a file is a documentation file based on extension."""
    return file_path.suffix.lower() in DOC_EXTENSIONS


def is_pdf(file_path: Path) -> bool:
    """Check if a file is a PDF based on extension."""
    return file_path.suffix.lower() in PDF_EXTENSIONS


def should_skip_directory(dir_name: str) -> bool:
    """Check if a directory should be skipped."""
    return dir_name in SKIP_DIRECTORIES or dir_name.startswith(".")


def should_skip_file(file_path: Path, mode: str = "smart") -> tuple[bool, str]:
    """
    Determine if a file should be skipped.

    Args:
        file_path: Path to check.
        mode: "smart" for code-only, "complete" for all text files.

    Returns:
        Tuple of (should_skip, reason).
    """
    filename = file_path.name
    filename_upper = filename.upper()
    suffix = file_path.suffix.lower()

    # BUG #1 FIX: Always skip lock files and system files
    if filename in SKIP_FILES or filename.lower() in {f.lower() for f in SKIP_FILES}:
        return True, "LOCK_OR_SYSTEM_FILE"

    # Always skip binary extensions
    if suffix in BINARY_EXTENSIONS:
        return True, "BINARY_EXTENSION"

    # PDF is handled specially (extracted)
    if suffix in PDF_EXTENSIONS:
        return False, ""

    # BUG #3 FIX: Keep important docs in Smart mode
    if filename_upper in KEEP_IN_SMART_MODE:
        return False, ""

    # Smart mode: only code files
    if mode == "smart":
        if suffix in CODE_EXTENSIONS:
            return False, ""
        if suffix in DOC_EXTENSIONS:
            return True, "DOC_IN_SMART_MODE"
        # Check if file has no extension but might be code (Makefile, Dockerfile, etc.)
        if not suffix:
            name = file_path.name.lower()
            if name in {"makefile", "dockerfile", "jenkinsfile", "vagrantfile", "gemfile",
                        "rakefile", "procfile", "brewfile", ".gitignore", ".dockerignore",
                        ".editorconfig", ".prettierrc", ".eslintrc"}:
                return False, ""
        return True, "NOT_CODE_FILE"

    # Complete mode: code + docs
    if mode == "complete":
        if suffix in CODE_EXTENSIONS or suffix in DOC_EXTENSIONS:
            return False, ""
        # Also include files without extension that might be text
        if not suffix:
            name = file_path.name.lower()
            if name in {"readme", "license", "changelog", "authors", "contributing",
                        "makefile", "dockerfile", "jenkinsfile", "vagrantfile"}:
                return False, ""
        return True, "NOT_TEXT_FILE"

    return True, "UNKNOWN_MODE"


def get_language_for_syntax(file_path: Path) -> str:
    """
    Get the markdown syntax highlighting language for a file.

    Args:
        file_path: Path to the file.

    Returns:
        Language identifier for markdown code blocks.
    """
    ext_to_lang = {
        # Python
        ".py": "python", ".pyi": "python", ".pyx": "python",
        # JavaScript
        ".js": "javascript", ".jsx": "javascript", ".mjs": "javascript", ".cjs": "javascript",
        # TypeScript
        ".ts": "typescript", ".tsx": "typescript",
        # Web
        ".html": "html", ".htm": "html",
        ".css": "css", ".scss": "scss", ".sass": "sass", ".less": "less",
        ".vue": "vue", ".svelte": "svelte",
        # Systems
        ".c": "c", ".h": "c",
        ".cpp": "cpp", ".hpp": "cpp", ".cc": "cpp", ".cxx": "cpp",
        # JVM
        ".java": "java", ".kt": "kotlin", ".scala": "scala", ".groovy": "groovy",
        # .NET
        ".cs": "csharp", ".fs": "fsharp", ".vb": "vb",
        # Go/Rust
        ".go": "go", ".rs": "rust",
        # Ruby/PHP
        ".rb": "ruby", ".php": "php",
        # Shell
        ".sh": "bash", ".bash": "bash", ".zsh": "zsh",
        ".ps1": "powershell", ".psm1": "powershell",
        ".bat": "batch", ".cmd": "batch",
        # Config
        ".json": "json", ".yaml": "yaml", ".yml": "yaml",
        ".toml": "toml", ".ini": "ini",
        # Data
        ".sql": "sql", ".graphql": "graphql",
        # Mobile
        ".swift": "swift", ".m": "objective-c",
        # Docs
        ".md": "markdown", ".markdown": "markdown",
        ".rst": "rst", ".txt": "text",
        # Other
        ".r": "r", ".R": "r", ".jl": "julia", ".lua": "lua",
        ".dart": "dart", ".elm": "elm", ".ex": "elixir", ".exs": "elixir",
        ".hs": "haskell", ".ml": "ocaml",
    }

    suffix = file_path.suffix.lower()
    return ext_to_lang.get(suffix, "")


def parse_gitignore(gitignore_path: Path) -> list[str]:
    """
    Parse a .gitignore file and return patterns.

    Args:
        gitignore_path: Path to the .gitignore file.

    Returns:
        List of patterns from the file.
    """
    patterns = []
    if not gitignore_path.exists():
        return patterns

    try:
        content = gitignore_path.read_text(encoding="utf-8")
        for line in content.splitlines():
            line = line.strip()
            # Skip empty lines and comments
            if not line or line.startswith("#"):
                continue
            patterns.append(line)
    except Exception:
        pass

    return patterns


def matches_gitignore(file_path: Path, root_path: Path, patterns: list[str]) -> bool:
    """
    Check if a file matches any gitignore pattern.

    Args:
        file_path: Path to the file.
        root_path: Root directory for relative path calculation.
        patterns: List of gitignore patterns.

    Returns:
        True if the file should be ignored.
    """
    import fnmatch

    try:
        rel_path = file_path.relative_to(root_path)
        rel_str = str(rel_path).replace("\\", "/")
    except ValueError:
        return False

    for pattern in patterns:
        # Handle negation patterns (we skip them for simplicity)
        if pattern.startswith("!"):
            continue

        # Handle directory-specific patterns
        if pattern.endswith("/"):
            pattern = pattern[:-1]
            # Check if any directory component matches
            for part in rel_path.parts[:-1]:
                if fnmatch.fnmatch(part, pattern):
                    return True
        else:
            # Check full path match
            if fnmatch.fnmatch(rel_str, pattern):
                return True
            # Check filename match
            if fnmatch.fnmatch(file_path.name, pattern):
                return True
            # Check with ** prefix for deep matching
            if fnmatch.fnmatch(rel_str, f"**/{pattern}"):
                return True

    return False


def collect_files(
    root_path: Path,
    mode: str = "smart",
    respect_gitignore: bool = True,
) -> list[Path]:
    """
    Collect all files to process from a directory.

    Args:
        root_path: Root directory to scan.
        mode: "smart" or "complete".
        respect_gitignore: Whether to respect .gitignore files.

    Returns:
        List of file paths to process.
    """
    files: list[Path] = []

    # Parse .gitignore if present and respect_gitignore is True
    gitignore_patterns: list[str] = []
    if respect_gitignore:
        gitignore_path = root_path / ".gitignore"
        gitignore_patterns = parse_gitignore(gitignore_path)

    for item in root_path.rglob("*"):
        # Skip directories in the path
        if any(should_skip_directory(part) for part in item.parts):
            continue

        if not item.is_file():
            continue

        # Check gitignore patterns
        if gitignore_patterns and matches_gitignore(item, root_path, gitignore_patterns):
            continue

        skip, _ = should_skip_file(item, mode)
        if skip:
            continue

        files.append(item)

    return sorted(files)
