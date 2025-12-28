"""Tests for core merge functionality."""

from pathlib import Path

import pytest

from turbomerger.core import FileProcessResult, MergeStats, StreamingMerger, merge_project


class TestMergeStats:
    """Tests for MergeStats dataclass."""

    def test_default_values(self):
        """Default stats should be zero."""
        stats = MergeStats()
        assert stats.total_files == 0
        assert stats.processed_files == 0
        assert stats.skipped_files == 0
        assert stats.total_chars == 0

    def test_skipped_reasons_dict(self):
        """Skipped reasons should be a dict."""
        stats = MergeStats()
        stats.skipped_reasons["BINARY"] = 5
        assert stats.skipped_reasons["BINARY"] == 5


class TestFileProcessResult:
    """Tests for FileProcessResult dataclass."""

    def test_default_values(self):
        """Default result should have empty content."""
        result = FileProcessResult(path=Path("test.py"), relative_path="test.py")
        assert result.content is None
        assert result.error is None
        assert result.is_skipped is False

    def test_with_content(self):
        """Result with content should track char count."""
        result = FileProcessResult(
            path=Path("test.py"),
            relative_path="test.py",
            content="print('hello')",
            char_count=14,
        )
        assert result.content == "print('hello')"
        assert result.char_count == 14


class TestStreamingMerger:
    """Tests for StreamingMerger class."""

    def test_initialization(self, sample_project, temp_dir):
        """Merger should initialize with correct settings."""
        output = temp_dir / "output.md"
        merger = StreamingMerger(
            project_root=sample_project,
            output_path=output,
            mode="smart",
        )
        assert merger.project_root == sample_project
        assert merger.output_path == output
        assert merger.mode == "smart"

    def test_merge_creates_output(self, sample_project, temp_dir):
        """Merge should create output file."""
        output = temp_dir / "output.md"
        merger = StreamingMerger(
            project_root=sample_project,
            output_path=output,
            mode="smart",
        )

        stats, results = merger.merge()

        assert output.exists()
        assert stats.processed_files > 0
        assert isinstance(results, list)

    def test_merge_includes_header(self, sample_project, temp_dir):
        """Output should include header."""
        output = temp_dir / "output.md"
        merger = StreamingMerger(
            project_root=sample_project,
            output_path=output,
        )
        merger.merge()

        content = output.read_text()
        assert "TurboMerger" in content
        assert sample_project.name in content

    def test_merge_includes_code(self, sample_project, temp_dir):
        """Output should include code content."""
        output = temp_dir / "output.md"
        merger = StreamingMerger(
            project_root=sample_project,
            output_path=output,
            mode="smart",
        )
        merger.merge()

        content = output.read_text()
        assert "def main():" in content
        assert "print('Hello')" in content

    def test_merge_respects_mode(self, sample_project, temp_dir):
        """Both modes should include README.md (BUG #3 fix: important docs kept in Smart Mode)."""
        output = temp_dir / "output.md"

        # Smart mode - now includes README.md (BUG #3 FIX)
        merger = StreamingMerger(
            project_root=sample_project,
            output_path=output,
            mode="smart",
        )
        merger.merge()
        smart_content = output.read_text()

        # Complete mode
        merger = StreamingMerger(
            project_root=sample_project,
            output_path=output,
            mode="complete",
        )
        merger.merge()
        complete_content = output.read_text()

        # Both modes should now include README.md (BUG #3 FIX)
        assert "Documentation here" in smart_content
        assert "Documentation here" in complete_content

    def test_progress_callback(self, sample_project, temp_dir):
        """Progress callback should be called."""
        output = temp_dir / "output.md"
        merger = StreamingMerger(
            project_root=sample_project,
            output_path=output,
        )

        progress_calls = []

        def callback(current, total, filename):
            progress_calls.append((current, total, filename))

        merger.merge(progress_callback=callback)

        assert len(progress_calls) > 0
        # Last call should have current == total
        last = progress_calls[-1]
        assert last[0] == last[1]

    def test_truncation(self, temp_dir):
        """Large files should be truncated."""
        # Create project with large file
        project = temp_dir / "project"
        project.mkdir()
        large_file = project / "large.py"
        large_file.write_text("x = " + "a" * 200000)  # 200k chars

        output = temp_dir / "output.md"
        merger = StreamingMerger(
            project_root=project,
            output_path=output,
            max_chars_per_file=1000,
        )

        stats, results = merger.merge()

        assert stats.truncated_files == 1
        content = output.read_text()
        assert "Truncated" in content


class TestMergeProject:
    """Tests for merge_project convenience function."""

    def test_returns_path_and_stats(self, sample_project):
        """Function should return output path, stats, and results."""
        output_path, stats, results = merge_project(sample_project)

        assert output_path.exists()
        assert isinstance(stats, MergeStats)
        assert stats.processed_files > 0
        assert isinstance(results, list)

        # Cleanup
        output_path.unlink()

    def test_custom_output_path(self, sample_project, temp_dir):
        """Custom output path should be used."""
        custom_output = temp_dir / "custom_output.md"

        output_path, stats, results = merge_project(
            sample_project,
            output_path=custom_output,
        )

        assert output_path == custom_output
        assert custom_output.exists()
