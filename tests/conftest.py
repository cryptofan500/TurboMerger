"""Pytest fixtures for TurboMerger tests."""

import tempfile
from pathlib import Path

import pytest


@pytest.fixture
def temp_dir():
    """Create a temporary directory for tests."""
    with tempfile.TemporaryDirectory() as tmpdir:
        yield Path(tmpdir)


@pytest.fixture
def sample_project(temp_dir):
    """Create a sample project structure for testing."""
    # Create directories
    (temp_dir / "src").mkdir()
    (temp_dir / "tests").mkdir()
    (temp_dir / "docs").mkdir()

    # Create sample files
    (temp_dir / "src" / "main.py").write_text("def main():\n    print('Hello')\n")
    (temp_dir / "src" / "utils.py").write_text("def helper():\n    return True\n")
    (temp_dir / "tests" / "test_main.py").write_text("def test_main():\n    assert True\n")
    (temp_dir / "docs" / "README.md").write_text("# Project\n\nDocumentation here.\n")
    (temp_dir / ".gitignore").write_text("__pycache__/\n*.pyc\n")

    return temp_dir


@pytest.fixture
def binary_file(temp_dir):
    """Create a binary file for testing."""
    binary_path = temp_dir / "binary.bin"
    binary_path.write_bytes(b"\x00\x01\x02\x03\x04\x05")
    return binary_path


@pytest.fixture
def large_file(temp_dir):
    """Create a large text file for testing."""
    large_path = temp_dir / "large.txt"
    large_path.write_text("x" * 500000)  # 500KB of text
    return large_path
