"""Tests for detector module."""

from pathlib import Path

import pytest

from turbomerger.detector import (
    collect_files,
    get_language_for_syntax,
    is_binary,
    is_code_file,
    is_doc_file,
    is_pdf,
    should_skip_directory,
    should_skip_file,
)


class TestIsBinary:
    """Tests for is_binary function."""

    def test_text_file(self, temp_dir):
        """Text files should not be detected as binary."""
        text_file = temp_dir / "test.txt"
        text_file.write_text("Hello, World!")
        assert is_binary(text_file) is False

    def test_binary_file(self, binary_file):
        """Files with null bytes should be detected as binary."""
        assert is_binary(binary_file) is True

    def test_unicode_file(self, temp_dir):
        """Unicode files should not be detected as binary."""
        unicode_file = temp_dir / "unicode.txt"
        unicode_file.write_text("Hello, 世界! 🌍", encoding="utf-8")
        assert is_binary(unicode_file) is False

    def test_nonexistent_file(self, temp_dir):
        """Nonexistent files should be treated as binary."""
        missing = temp_dir / "missing.txt"
        assert is_binary(missing) is True


class TestIsCodeFile:
    """Tests for is_code_file function."""

    @pytest.mark.parametrize(
        "filename",
        [
            "main.py",
            "app.js",
            "index.ts",
            "Component.tsx",
            "main.go",
            "lib.rs",
            "App.java",
            "main.c",
            "main.cpp",
        ],
    )
    def test_code_files(self, filename):
        """Common code files should be detected."""
        assert is_code_file(Path(filename)) is True

    @pytest.mark.parametrize(
        "filename",
        [
            "README.md",
            "doc.txt",
            "image.png",
            "data.csv",
        ],
    )
    def test_non_code_files(self, filename):
        """Non-code files should not be detected as code."""
        assert is_code_file(Path(filename)) is False


class TestIsDocFile:
    """Tests for is_doc_file function."""

    @pytest.mark.parametrize(
        "filename",
        [
            "README.md",
            "docs.markdown",
            "guide.rst",
            "notes.txt",
        ],
    )
    def test_doc_files(self, filename):
        """Documentation files should be detected."""
        assert is_doc_file(Path(filename)) is True

    @pytest.mark.parametrize(
        "filename",
        [
            "main.py",
            "app.js",
            "image.png",
        ],
    )
    def test_non_doc_files(self, filename):
        """Non-doc files should not be detected as docs."""
        assert is_doc_file(Path(filename)) is False


class TestIsPdf:
    """Tests for is_pdf function."""

    def test_pdf_file(self):
        """PDF files should be detected."""
        assert is_pdf(Path("document.pdf")) is True
        assert is_pdf(Path("document.PDF")) is True

    def test_non_pdf_file(self):
        """Non-PDF files should not be detected."""
        assert is_pdf(Path("document.doc")) is False
        assert is_pdf(Path("main.py")) is False


class TestShouldSkipDirectory:
    """Tests for should_skip_directory function."""

    @pytest.mark.parametrize(
        "dirname",
        [
            ".git",
            "node_modules",
            "__pycache__",
            "venv",
            ".venv",
            "dist",
            "build",
        ],
    )
    def test_skip_directories(self, dirname):
        """Common skip directories should be detected."""
        assert should_skip_directory(dirname) is True

    @pytest.mark.parametrize(
        "dirname",
        [
            "src",
            "lib",
            "tests",
            "app",
        ],
    )
    def test_keep_directories(self, dirname):
        """Normal directories should not be skipped."""
        assert should_skip_directory(dirname) is False


class TestShouldSkipFile:
    """Tests for should_skip_file function."""

    def test_smart_mode_code_files(self):
        """Code files should not be skipped in smart mode."""
        skip, reason = should_skip_file(Path("main.py"), mode="smart")
        assert skip is False

    def test_smart_mode_doc_files(self):
        """Doc files should be skipped in smart mode (except important ones like README)."""
        # Regular doc files should be skipped
        skip, reason = should_skip_file(Path("notes.md"), mode="smart")
        assert skip is True
        assert reason == "DOC_IN_SMART_MODE"

    def test_smart_mode_keeps_readme(self):
        """README.md should be kept in smart mode (BUG #3 fix)."""
        skip, reason = should_skip_file(Path("README.md"), mode="smart")
        assert skip is False  # README is kept in Smart Mode

    def test_complete_mode_doc_files(self):
        """Doc files should not be skipped in complete mode."""
        skip, reason = should_skip_file(Path("README.md"), mode="complete")
        assert skip is False

    def test_binary_files_always_skipped(self):
        """Binary files should always be skipped."""
        skip, reason = should_skip_file(Path("image.png"), mode="smart")
        assert skip is True
        assert reason == "BINARY_EXTENSION"

        skip, reason = should_skip_file(Path("image.png"), mode="complete")
        assert skip is True


class TestGetLanguageForSyntax:
    """Tests for get_language_for_syntax function."""

    @pytest.mark.parametrize(
        "filename,expected",
        [
            ("main.py", "python"),
            ("app.js", "javascript"),
            ("index.ts", "typescript"),
            ("style.css", "css"),
            ("main.go", "go"),
            ("lib.rs", "rust"),
            ("README.md", "markdown"),
            ("unknown.xyz", ""),
        ],
    )
    def test_language_detection(self, filename, expected):
        """Language should be correctly detected from extension."""
        assert get_language_for_syntax(Path(filename)) == expected


class TestCollectFiles:
    """Tests for collect_files function."""

    def test_smart_mode(self, sample_project):
        """Smart mode should collect code files plus important docs like README."""
        files = collect_files(sample_project, mode="smart")
        names = {f.name for f in files}

        assert "main.py" in names
        assert "utils.py" in names
        assert "test_main.py" in names
        # BUG #3 FIX: README.md is now KEPT in Smart Mode as it's an important doc
        assert "README.md" in names

    def test_complete_mode(self, sample_project):
        """Complete mode should include docs."""
        files = collect_files(sample_project, mode="complete")
        names = {f.name for f in files}

        assert "main.py" in names
        assert "README.md" in names

    def test_skips_git_directory(self, sample_project):
        """Git directory should be skipped."""
        git_dir = sample_project / ".git"
        git_dir.mkdir()
        (git_dir / "config").write_text("git config")

        files = collect_files(sample_project, mode="smart")
        paths = [str(f) for f in files]

        assert not any(".git" in p for p in paths)
