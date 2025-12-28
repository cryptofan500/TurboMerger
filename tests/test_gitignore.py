"""Tests for .gitignore parsing functionality."""

from pathlib import Path

import pytest

from turbomerger.detector import (
    collect_files,
    matches_gitignore,
    parse_gitignore,
)


class TestParseGitignore:
    """Tests for parse_gitignore function."""

    def test_parse_simple_patterns(self, temp_dir):
        """Simple patterns should be parsed."""
        gitignore = temp_dir / ".gitignore"
        gitignore.write_text("*.log\n*.tmp\nbuild/\n")

        patterns = parse_gitignore(gitignore)
        assert "*.log" in patterns
        assert "*.tmp" in patterns
        assert "build/" in patterns

    def test_skip_comments(self, temp_dir):
        """Comments should be skipped."""
        gitignore = temp_dir / ".gitignore"
        gitignore.write_text("# This is a comment\n*.log\n# Another comment\n")

        patterns = parse_gitignore(gitignore)
        assert len(patterns) == 1
        assert "*.log" in patterns

    def test_skip_empty_lines(self, temp_dir):
        """Empty lines should be skipped."""
        gitignore = temp_dir / ".gitignore"
        gitignore.write_text("*.log\n\n\n*.tmp\n")

        patterns = parse_gitignore(gitignore)
        assert len(patterns) == 2

    def test_nonexistent_file(self, temp_dir):
        """Nonexistent file should return empty list."""
        gitignore = temp_dir / ".gitignore"
        patterns = parse_gitignore(gitignore)
        assert patterns == []


class TestMatchesGitignore:
    """Tests for matches_gitignore function."""

    def test_wildcard_extension(self, temp_dir):
        """Wildcard extension patterns should match."""
        patterns = ["*.log"]
        log_file = temp_dir / "app.log"
        log_file.touch()

        assert matches_gitignore(log_file, temp_dir, patterns) is True

    def test_wildcard_no_match(self, temp_dir):
        """Non-matching files should not match."""
        patterns = ["*.log"]
        py_file = temp_dir / "app.py"
        py_file.touch()

        assert matches_gitignore(py_file, temp_dir, patterns) is False

    def test_directory_pattern(self, temp_dir):
        """Directory patterns should match files in directory."""
        patterns = ["build/"]
        build_dir = temp_dir / "build"
        build_dir.mkdir()
        build_file = build_dir / "output.js"
        build_file.touch()

        assert matches_gitignore(build_file, temp_dir, patterns) is True

    def test_filename_match(self, temp_dir):
        """Exact filename patterns should match."""
        patterns = [".env"]
        env_file = temp_dir / ".env"
        env_file.touch()

        assert matches_gitignore(env_file, temp_dir, patterns) is True

    def test_negation_patterns_ignored(self, temp_dir):
        """Negation patterns should be ignored (not supported)."""
        patterns = ["*.log", "!important.log"]
        log_file = temp_dir / "important.log"
        log_file.touch()

        # Since we don't support negation, the file should still match *.log
        assert matches_gitignore(log_file, temp_dir, patterns) is True


class TestCollectFilesWithGitignore:
    """Tests for collect_files respecting .gitignore."""

    def test_respects_gitignore(self, temp_dir):
        """collect_files should respect .gitignore patterns."""
        # Create gitignore
        gitignore = temp_dir / ".gitignore"
        gitignore.write_text("*.log\n*.tmp\n")

        # Create files
        (temp_dir / "main.py").write_text("print('hello')")
        (temp_dir / "app.log").write_text("log content")
        (temp_dir / "temp.tmp").write_text("temp content")

        files = collect_files(temp_dir, mode="smart", respect_gitignore=True)
        names = [f.name for f in files]

        assert "main.py" in names
        assert "app.log" not in names
        assert "temp.tmp" not in names

    def test_can_disable_gitignore(self, temp_dir):
        """collect_files should allow disabling gitignore."""
        # Create gitignore that would exclude the file
        gitignore = temp_dir / ".gitignore"
        gitignore.write_text("*.py\n")

        # Create file
        (temp_dir / "main.py").write_text("print('hello')")

        # With gitignore respect
        files_with = collect_files(temp_dir, mode="smart", respect_gitignore=True)
        assert len(files_with) == 0

        # Without gitignore respect
        files_without = collect_files(temp_dir, mode="smart", respect_gitignore=False)
        assert len(files_without) == 1
        assert files_without[0].name == "main.py"

    def test_no_gitignore_file(self, temp_dir):
        """collect_files should work when no .gitignore exists."""
        (temp_dir / "main.py").write_text("print('hello')")

        files = collect_files(temp_dir, mode="smart", respect_gitignore=True)
        assert len(files) == 1
        assert files[0].name == "main.py"
