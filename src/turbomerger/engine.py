"""Multithreaded Processing Engine for TurboMerger.

Design Principles:
- Main thread owns the GUI (Tkinter is NOT thread-safe)
- Workers process files and put results in queues
- Main thread polls queues via root.after(100, poll)
- Cancellation is cooperative (workers check event flag)
"""

import os
import queue
import threading
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, Optional

from turbomerger.detector import (
    collect_files,
    get_language_for_syntax,
    is_binary,
    is_pdf,
    should_skip_file,
)
from turbomerger.pdf_reader import extract_pdf_text
from turbomerger.sentinel import SecretMatch, check_for_secrets, is_secret_file


@dataclass
class FileResult:
    """Result from processing a single file."""

    path: Path
    relative_path: str = ""
    content: Optional[str] = None
    error: Optional[str] = None
    is_skipped: bool = False
    skip_reason: str = ""
    size_bytes: int = 0
    is_secret: bool = False
    secret_types: list[str] = field(default_factory=list)
    secret_matches: list[SecretMatch] = field(default_factory=list)
    is_truncated: bool = False
    is_pdf: bool = False
    char_count: int = 0
    language: str = ""


class ProcessingEngine:
    """Multithreaded file processing engine with cooperative cancellation."""

    def __init__(self, max_workers: Optional[int] = None):
        """
        Initialize the processing engine.

        Args:
            max_workers: Maximum worker threads (default: min(cpu_count, 16)).
        """
        self.max_workers = max_workers or min(os.cpu_count() or 4, 16)
        self.cancel_event = threading.Event()
        self.result_queue: queue.Queue[FileResult] = queue.Queue()
        self.progress_queue: queue.Queue[tuple] = queue.Queue()
        self._executor: Optional[ThreadPoolExecutor] = None
        self._worker_thread: Optional[threading.Thread] = None
        self._is_running = False

    @property
    def is_running(self) -> bool:
        """Check if processing is currently running."""
        return self._is_running

    def process_files(
        self,
        project_root: Path,
        mode: str = "smart",
        max_file_mb: float = 2.0,
        max_chars: int = 100_000,
        on_complete: Optional[Callable[[], None]] = None,
    ) -> None:
        """
        Process files in parallel. Non-blocking.

        Args:
            project_root: Root directory to process.
            mode: "smart" or "complete" scan mode.
            max_file_mb: Maximum file size in MB.
            max_chars: Maximum characters per file.
            on_complete: Callback when processing completes.
        """
        self.cancel_event.clear()
        self._is_running = True

        # Collect files first
        files = collect_files(project_root, mode)
        total_files = len(files)

        self.progress_queue.put(("start", total_files, 0, "Collecting files..."))

        def worker():
            try:
                with ThreadPoolExecutor(max_workers=self.max_workers) as executor:
                    self._executor = executor

                    # Submit all tasks
                    futures = {
                        executor.submit(
                            self._process_single,
                            f,
                            project_root,
                            mode,
                            max_file_mb,
                            max_chars,
                        ): f
                        for f in files
                    }

                    completed = 0
                    for future in as_completed(futures):
                        if self.cancel_event.is_set():
                            executor.shutdown(wait=False, cancel_futures=True)
                            self.progress_queue.put(("cancelled", total_files, completed, ""))
                            break

                        try:
                            result = future.result(timeout=30)
                            self.result_queue.put(result)
                            completed += 1
                            self.progress_queue.put(
                                ("progress", total_files, completed, result.path.name)
                            )
                        except Exception as e:
                            file_path = futures[future]
                            error_result = FileResult(
                                path=file_path,
                                relative_path=str(file_path.relative_to(project_root)),
                                error=str(e),
                                is_skipped=True,
                                skip_reason="PROCESSING_ERROR",
                            )
                            self.result_queue.put(error_result)
                            completed += 1
                            self.progress_queue.put(
                                ("progress", total_files, completed, file_path.name)
                            )

                    self._executor = None

                if not self.cancel_event.is_set():
                    self.progress_queue.put(("complete", total_files, total_files, ""))
                    if on_complete:
                        on_complete()

            finally:
                self._is_running = False

        self._worker_thread = threading.Thread(target=worker, daemon=True)
        self._worker_thread.start()

    def cancel(self) -> None:
        """
        Request cooperative cancellation.

        Workers check the cancel event and stop as soon as possible.
        This should respond within ~1 second.
        """
        self.cancel_event.set()
        if self._executor:
            self._executor.shutdown(wait=False, cancel_futures=True)

    def _process_single(
        self,
        file_path: Path,
        project_root: Path,
        mode: str,
        max_file_mb: float,
        max_chars: int,
    ) -> FileResult:
        """
        Process a single file. Runs in worker thread.

        Args:
            file_path: Path to the file.
            project_root: Project root for relative paths.
            mode: Scan mode.
            max_file_mb: Max file size in MB.
            max_chars: Max characters per file.

        Returns:
            FileResult with processing outcome.
        """
        rel_path = str(file_path.relative_to(project_root))
        result = FileResult(
            path=file_path,
            relative_path=rel_path,
            language=get_language_for_syntax(file_path),
        )

        # Check cancellation early
        if self.cancel_event.is_set():
            result.is_skipped = True
            result.skip_reason = "CANCELLED"
            return result

        # Check if file should be skipped based on mode
        skip, reason = should_skip_file(file_path, mode)
        if skip:
            result.is_skipped = True
            result.skip_reason = reason
            return result

        # Check if it's a secret file (by name)
        is_secret, secret_reason = is_secret_file(file_path)
        if is_secret:
            result.is_skipped = True
            result.is_secret = True
            result.skip_reason = secret_reason
            result.secret_types = [secret_reason]
            return result

        # Get file size
        try:
            result.size_bytes = file_path.stat().st_size
        except OSError as e:
            result.error = f"Cannot access: {e}"
            result.is_skipped = True
            result.skip_reason = "ACCESS_ERROR"
            return result

        # Check size limit
        max_bytes = int(max_file_mb * 1024 * 1024)
        if result.size_bytes > max_bytes:
            result.is_skipped = True
            result.skip_reason = "TOO_LARGE"
            return result

        # Check cancellation before heavy I/O
        if self.cancel_event.is_set():
            result.is_skipped = True
            result.skip_reason = "CANCELLED"
            return result

        # Handle PDF files
        if is_pdf(file_path):
            result.is_pdf = True
            content = extract_pdf_text(file_path)
            if content:
                result.content = content
                result.char_count = len(content)
            else:
                result.is_skipped = True
                result.skip_reason = "PDF_EXTRACTION_FAILED"
            return result

        # Check if binary
        if is_binary(file_path):
            result.is_skipped = True
            result.skip_reason = "BINARY"
            return result

        # Read text file
        try:
            with open(file_path, "r", encoding="utf-8", errors="replace") as f:
                # Read in chunks, checking for cancellation
                chunks = []
                total_read = 0
                chunk_size = 65536  # 64KB chunks

                while total_read < max_chars + 1:
                    if self.cancel_event.is_set():
                        result.is_skipped = True
                        result.skip_reason = "CANCELLED"
                        return result

                    chunk = f.read(chunk_size)
                    if not chunk:
                        break
                    chunks.append(chunk)
                    total_read += len(chunk)

                content = "".join(chunks)

                # Truncate if needed
                if len(content) > max_chars:
                    content = content[:max_chars]
                    result.is_truncated = True

                result.content = content
                result.char_count = len(content)

        except Exception as e:
            result.error = str(e)
            result.is_skipped = True
            result.skip_reason = "READ_ERROR"
            return result

        # Scan for secrets in content
        if result.content:
            secret_result = check_for_secrets(result.content, file_path)
            if secret_result.has_secrets:
                result.is_secret = True
                result.secret_matches = secret_result.matches
                result.secret_types = list({m.secret_type for m in secret_result.matches})

        return result

    def get_results(self, timeout: float = 0.1) -> list[FileResult]:
        """
        Get all available results from the queue.

        Args:
            timeout: How long to wait for results.

        Returns:
            List of available FileResult objects.
        """
        results = []
        try:
            while True:
                result = self.result_queue.get_nowait()
                results.append(result)
        except queue.Empty:
            pass
        return results

    def get_progress(self) -> Optional[tuple]:
        """
        Get latest progress update.

        Returns:
            Tuple of (status, total, completed, current_file) or None.
        """
        latest = None
        try:
            while True:
                latest = self.progress_queue.get_nowait()
        except queue.Empty:
            pass
        return latest
