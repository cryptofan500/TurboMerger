"""Tests for engine module (multithreaded processing)."""

import time
from pathlib import Path

import pytest

from turbomerger.engine import FileResult, ProcessingEngine


class TestFileResult:
    """Tests for FileResult dataclass."""

    def test_default_values(self):
        """Default values should be set correctly."""
        result = FileResult(path=Path("test.py"))
        assert result.content is None
        assert result.error is None
        assert result.is_skipped is False
        assert result.is_secret is False
        assert result.secret_types == []

    def test_with_content(self):
        """Result with content should have correct values."""
        result = FileResult(
            path=Path("test.py"),
            relative_path="test.py",
            content="print('hello')",
            char_count=14,
        )
        assert result.content == "print('hello')"
        assert result.char_count == 14

    def test_with_secret(self):
        """Result with secret should be flagged."""
        result = FileResult(
            path=Path("config.py"),
            is_secret=True,
            secret_types=["AWS Access Key"],
        )
        assert result.is_secret is True
        assert "AWS Access Key" in result.secret_types


class TestProcessingEngine:
    """Tests for ProcessingEngine class."""

    def test_initialization(self):
        """Engine should initialize with default workers."""
        engine = ProcessingEngine()
        assert engine.max_workers > 0
        assert engine.max_workers <= 16
        assert engine.is_running is False

    def test_custom_workers(self):
        """Engine should accept custom worker count."""
        engine = ProcessingEngine(max_workers=4)
        assert engine.max_workers == 4

    def test_process_files(self, sample_project):
        """Engine should process files correctly."""
        engine = ProcessingEngine(max_workers=2)

        completed = []

        def on_complete():
            completed.append(True)

        engine.process_files(
            project_root=sample_project,
            mode="smart",
            on_complete=on_complete,
        )

        # Wait for processing
        timeout = 10  # seconds
        start = time.time()
        while engine.is_running and time.time() - start < timeout:
            time.sleep(0.1)

        # Check results
        results = []
        while True:
            batch = engine.get_results()
            if not batch:
                break
            results.extend(batch)

        assert len(results) > 0
        # Should have processed Python files
        py_results = [r for r in results if r.path.suffix == ".py"]
        assert len(py_results) >= 2  # main.py, utils.py, test_main.py

    def test_cancel(self, temp_dir):
        """Engine should support cancellation."""
        # Create many files to ensure we have time to cancel
        for i in range(100):
            (temp_dir / f"file_{i}.py").write_text(f"# File {i}\n" * 100)

        engine = ProcessingEngine(max_workers=2)

        engine.process_files(
            project_root=temp_dir,
            mode="smart",
        )

        # Cancel immediately
        engine.cancel()

        # Wait a bit
        time.sleep(0.5)

        # Check that cancellation was effective
        assert engine.cancel_event.is_set()

    def test_secret_detection(self, temp_dir):
        """Engine should detect secrets in files."""
        # Create file with secret - AWS key needs prefix + 16 chars = 20 total
        secret_file = temp_dir / "config.py"
        secret_file.write_text('AWS_KEY = "AKIAIOSFODNN7EXAMPLA"')

        engine = ProcessingEngine(max_workers=2)
        engine.process_files(project_root=temp_dir, mode="smart")

        # Wait for processing
        timeout = 10
        start = time.time()
        while engine.is_running and time.time() - start < timeout:
            time.sleep(0.1)

        # Get results
        results = []
        while True:
            batch = engine.get_results()
            if not batch:
                break
            results.extend(batch)

        # Find the config.py result
        config_result = next((r for r in results if r.path.name == "config.py"), None)
        assert config_result is not None
        assert config_result.is_secret is True

    def test_env_file_not_collected(self, temp_dir):
        """Engine should not collect .env files (filtered by collect_files)."""
        # Create .env file - .env files don't have code extensions so they're
        # not collected by collect_files in smart mode
        env_file = temp_dir / ".env"
        env_file.write_text("SECRET=value")

        # Create a normal file so we have something to process
        normal_file = temp_dir / "main.py"
        normal_file.write_text("print('hello')")

        engine = ProcessingEngine(max_workers=2)
        engine.process_files(project_root=temp_dir, mode="smart")

        # Wait for processing
        timeout = 10
        start = time.time()
        while engine.is_running and time.time() - start < timeout:
            time.sleep(0.1)

        # Get results
        results = []
        while True:
            batch = engine.get_results()
            if not batch:
                break
            results.extend(batch)

        # .env should NOT be in results (not collected)
        env_result = next((r for r in results if r.path.name == ".env"), None)
        assert env_result is None

        # But main.py should be processed
        main_result = next((r for r in results if r.path.name == "main.py"), None)
        assert main_result is not None

    def test_binary_file_skipped(self, temp_dir):
        """Engine should skip binary files."""
        # Create binary file
        binary_file = temp_dir / "data.bin"
        binary_file.write_bytes(b"\x00\x01\x02\x03")

        # Create a py file with .bin extension test
        py_file = temp_dir / "test.py"
        py_file.write_text("print('test')")

        engine = ProcessingEngine(max_workers=2)
        engine.process_files(project_root=temp_dir, mode="smart")

        # Wait for processing
        timeout = 10
        start = time.time()
        while engine.is_running and time.time() - start < timeout:
            time.sleep(0.1)

        # Get results
        results = []
        while True:
            batch = engine.get_results()
            if not batch:
                break
            results.extend(batch)

        # Binary should not be in results (filtered by extension in collect_files)
        # or skipped as binary
        bin_results = [r for r in results if r.path.suffix == ".bin"]
        for r in bin_results:
            assert r.is_skipped is True

    def test_progress_updates(self, sample_project):
        """Engine should provide progress updates."""
        engine = ProcessingEngine(max_workers=2)
        engine.process_files(project_root=sample_project, mode="smart")

        # Collect progress updates
        progress_updates = []
        timeout = 10
        start = time.time()

        while time.time() - start < timeout:
            progress = engine.get_progress()
            if progress:
                progress_updates.append(progress)
                if progress[0] == "complete":
                    break
            time.sleep(0.1)

        # Should have some progress updates
        assert len(progress_updates) > 0

        # Should end with complete
        assert progress_updates[-1][0] == "complete"

    def test_truncation(self, temp_dir):
        """Engine should truncate large files."""
        # Create large file
        large_file = temp_dir / "large.py"
        large_file.write_text("x = " + "a" * 200000)  # 200k chars

        engine = ProcessingEngine(max_workers=2)
        engine.process_files(
            project_root=temp_dir,
            mode="smart",
            max_chars=1000,  # Low limit
        )

        # Wait for processing
        timeout = 10
        start = time.time()
        while engine.is_running and time.time() - start < timeout:
            time.sleep(0.1)

        # Get results
        results = []
        while True:
            batch = engine.get_results()
            if not batch:
                break
            results.extend(batch)

        # Find large.py result
        large_result = next((r for r in results if r.path.name == "large.py"), None)
        assert large_result is not None
        assert large_result.is_truncated is True
        assert large_result.char_count <= 1000

    def test_mode_filtering(self, sample_project):
        """Engine should include README.md in both modes (BUG #3 fix)."""
        # Smart mode - now includes README.md (BUG #3 FIX)
        engine_smart = ProcessingEngine(max_workers=2)
        engine_smart.process_files(project_root=sample_project, mode="smart")

        timeout = 10
        start = time.time()
        while engine_smart.is_running and time.time() - start < timeout:
            time.sleep(0.1)

        smart_results = []
        while True:
            batch = engine_smart.get_results()
            if not batch:
                break
            smart_results.extend(batch)

        # Complete mode - includes docs
        engine_complete = ProcessingEngine(max_workers=2)
        engine_complete.process_files(project_root=sample_project, mode="complete")

        start = time.time()
        while engine_complete.is_running and time.time() - start < timeout:
            time.sleep(0.1)

        complete_results = []
        while True:
            batch = engine_complete.get_results()
            if not batch:
                break
            complete_results.extend(batch)

        # Complete mode should have README.md processed
        readme_in_complete = any(
            r.path.name == "README.md" and not r.is_skipped
            for r in complete_results
        )

        # Smart mode should ALSO have README.md (BUG #3 FIX - important docs kept)
        readme_in_smart = any(
            r.path.name == "README.md" and not r.is_skipped
            for r in smart_results
        )

        assert readme_in_complete is True
        assert readme_in_smart is True  # BUG #3 FIX: README.md kept in Smart Mode
