"""CustomTkinter GUI for TurboMerger with Dual Processing Modes.

Safe Mode: Single-threaded streaming using core.py (DEFAULT - most reliable)
Turbo Mode: Multi-threaded parallel using engine.py (faster for large projects)
"""

import json
import os
import subprocess
import sys
import threading
from datetime import datetime
from pathlib import Path
from tkinter import filedialog
from typing import Optional

import customtkinter as ctk

from turbomerger import __version__
from turbomerger.core import FileProcessResult, StreamingMerger
from turbomerger.engine import FileResult, ProcessingEngine

# Token counting with tiktoken
try:
    import tiktoken
    TIKTOKEN_AVAILABLE = True
except ImportError:
    TIKTOKEN_AVAILABLE = False

# Config profiles directory
CONFIG_DIR = Path.home() / ".turbomerger"
PROFILES_FILE = CONFIG_DIR / "profiles.json"


def count_tokens(text: str, model: str = "cl100k_base") -> int:
    """
    Count tokens using tiktoken.

    Args:
        text: Text to count tokens for.
        model: Tokenizer model to use (cl100k_base for GPT-4/Claude).

    Returns:
        Number of tokens, or -1 if tiktoken unavailable.
    """
    if not TIKTOKEN_AVAILABLE:
        return -1
    try:
        encoding = tiktoken.get_encoding(model)
        return len(encoding.encode(text))
    except Exception:
        return -1


def load_profiles() -> dict:
    """Load saved configuration profiles."""
    if PROFILES_FILE.exists():
        try:
            return json.loads(PROFILES_FILE.read_text(encoding="utf-8"))
        except Exception:
            return {}
    return {}


def save_profiles(profiles: dict) -> None:
    """Save configuration profiles."""
    CONFIG_DIR.mkdir(parents=True, exist_ok=True)
    PROFILES_FILE.write_text(json.dumps(profiles, indent=2), encoding="utf-8")


class TurboMergerApp(ctk.CTk):
    """Main TurboMerger application window."""

    def __init__(self):
        super().__init__()

        # Configure appearance
        ctk.set_appearance_mode("system")
        ctk.set_default_color_theme("blue")

        # Window setup
        self.title(f"TurboMerger v{__version__}")
        self.geometry("750x650")
        self.minsize(650, 550)

        # State
        self.project_path: Optional[Path] = None
        self.output_path: Optional[Path] = None
        self.is_processing = False
        self.cancel_requested = False

        # Turbo Mode engine
        self.engine = ProcessingEngine()

        # Safe Mode thread and results
        self._safe_mode_thread: Optional[threading.Thread] = None
        self._safe_mode_cancelled = threading.Event()

        # Results storage (unified for both modes)
        self.processed_results: list = []
        self.secret_files: list = []
        self.skipped_files: list = []

        # Build UI
        self._create_widgets()

        # Handle command line argument (folder passed via context menu)
        if len(sys.argv) > 1:
            path = Path(sys.argv[1])
            if path.exists():
                if path.is_file():
                    path = path.parent
                self._set_project_path(path)

        # Start queue polling for Turbo Mode
        self._poll_engine()

    def _create_widgets(self):
        """Create all UI widgets."""
        # Configure grid
        self.grid_columnconfigure(0, weight=1)
        self.grid_rowconfigure(4, weight=1)

        # === Header ===
        header_frame = ctk.CTkFrame(self, fg_color="transparent")
        header_frame.grid(row=0, column=0, padx=20, pady=(20, 10), sticky="ew")
        header_frame.grid_columnconfigure(0, weight=1)

        title_label = ctk.CTkLabel(
            header_frame,
            text=f"TurboMerger v{__version__}",
            font=ctk.CTkFont(size=24, weight="bold"),
        )
        title_label.grid(row=0, column=0, sticky="w")

        subtitle_label = ctk.CTkLabel(
            header_frame,
            text="Merge your codebase for AI assistants",
            font=ctk.CTkFont(size=12),
            text_color="gray",
        )
        subtitle_label.grid(row=1, column=0, sticky="w")

        # === Folder Selection ===
        folder_frame = ctk.CTkFrame(self)
        folder_frame.grid(row=1, column=0, padx=20, pady=10, sticky="ew")
        folder_frame.grid_columnconfigure(1, weight=1)

        folder_label = ctk.CTkLabel(folder_frame, text="Project Folder:")
        folder_label.grid(row=0, column=0, padx=10, pady=10, sticky="w")

        self.folder_entry = ctk.CTkEntry(folder_frame, placeholder_text="Select a folder...")
        self.folder_entry.grid(row=0, column=1, padx=5, pady=10, sticky="ew")

        self.browse_btn = ctk.CTkButton(
            folder_frame, text="Browse", width=80, command=self._browse_folder
        )
        self.browse_btn.grid(row=0, column=2, padx=10, pady=10)

        # === Options Frame (Scan Mode + Processing Mode) ===
        options_frame = ctk.CTkFrame(self)
        options_frame.grid(row=2, column=0, padx=20, pady=10, sticky="ew")
        options_frame.grid_columnconfigure(1, weight=1)

        # Scan Mode section
        scan_label = ctk.CTkLabel(options_frame, text="Scan Mode:", font=ctk.CTkFont(weight="bold"))
        scan_label.grid(row=0, column=0, padx=10, pady=(10, 5), sticky="w")

        self.mode_var = ctk.StringVar(value="smart")

        smart_radio = ctk.CTkRadioButton(
            options_frame,
            text="Smart Code Scan (code files only)",
            variable=self.mode_var,
            value="smart",
        )
        smart_radio.grid(row=1, column=0, padx=30, pady=2, sticky="w")

        complete_radio = ctk.CTkRadioButton(
            options_frame,
            text="Complete Scan (all text files)",
            variable=self.mode_var,
            value="complete",
        )
        complete_radio.grid(row=2, column=0, padx=30, pady=(2, 10), sticky="w")

        # Processing Mode section
        proc_label = ctk.CTkLabel(options_frame, text="Processing Mode:", font=ctk.CTkFont(weight="bold"))
        proc_label.grid(row=0, column=1, padx=10, pady=(10, 5), sticky="w")

        self.processing_mode_var = ctk.StringVar(value="safe")  # DEFAULT: Safe Mode

        safe_radio = ctk.CTkRadioButton(
            options_frame,
            text="Safe Mode (Recommended)",
            variable=self.processing_mode_var,
            value="safe",
        )
        safe_radio.grid(row=1, column=1, padx=30, pady=2, sticky="w")

        turbo_radio = ctk.CTkRadioButton(
            options_frame,
            text="Turbo Mode (Faster)",
            variable=self.processing_mode_var,
            value="turbo",
        )
        turbo_radio.grid(row=2, column=1, padx=30, pady=(2, 10), sticky="w")

        # Mode descriptions
        safe_desc = ctk.CTkLabel(
            options_frame,
            text="Single-threaded, most reliable",
            font=ctk.CTkFont(size=10),
            text_color="gray",
        )
        safe_desc.grid(row=1, column=1, padx=(180, 0), pady=2, sticky="w")

        turbo_desc = ctk.CTkLabel(
            options_frame,
            text="Multi-threaded, faster for large projects",
            font=ctk.CTkFont(size=10),
            text_color="gray",
        )
        turbo_desc.grid(row=2, column=1, padx=(180, 0), pady=(2, 10), sticky="w")

        # === Tabview ===
        self.tabview = ctk.CTkTabview(self)
        self.tabview.grid(row=4, column=0, padx=20, pady=10, sticky="nsew")

        # Progress Tab
        self.progress_tab = self.tabview.add("Progress")
        self.progress_tab.grid_columnconfigure(0, weight=1)
        self.progress_tab.grid_rowconfigure(2, weight=1)

        self.progress_bar = ctk.CTkProgressBar(self.progress_tab)
        self.progress_bar.grid(row=0, column=0, padx=20, pady=(20, 10), sticky="ew")
        self.progress_bar.set(0)

        self.progress_label = ctk.CTkLabel(
            self.progress_tab, text="Ready to merge", font=ctk.CTkFont(size=12)
        )
        self.progress_label.grid(row=1, column=0, padx=20, pady=5, sticky="w")

        self.status_label = ctk.CTkLabel(
            self.progress_tab, text="", font=ctk.CTkFont(size=11), text_color="gray"
        )
        self.status_label.grid(row=2, column=0, padx=20, pady=5, sticky="nw")

        # Security Tab
        self.security_tab = self.tabview.add("Security")
        self.security_tab.grid_columnconfigure(0, weight=1)
        self.security_tab.grid_rowconfigure(1, weight=1)

        security_header = ctk.CTkLabel(
            self.security_tab,
            text="Security Scan Results",
            font=ctk.CTkFont(size=14, weight="bold"),
        )
        security_header.grid(row=0, column=0, padx=20, pady=(10, 5), sticky="w")

        self.security_text = ctk.CTkTextbox(
            self.security_tab, font=ctk.CTkFont(family="Consolas", size=11)
        )
        self.security_text.grid(row=1, column=0, padx=10, pady=10, sticky="nsew")
        self.security_text.insert("end", "No scan performed yet.\n\nRun a merge to scan for secrets.")

        # Log Tab
        self.log_tab = self.tabview.add("Log")
        self.log_tab.grid_columnconfigure(0, weight=1)
        self.log_tab.grid_rowconfigure(0, weight=1)

        self.log_text = ctk.CTkTextbox(self.log_tab, font=ctk.CTkFont(family="Consolas", size=11))
        self.log_text.grid(row=0, column=0, padx=10, pady=10, sticky="nsew")

        # === Button Bar ===
        button_frame = ctk.CTkFrame(self, fg_color="transparent")
        button_frame.grid(row=5, column=0, padx=20, pady=(10, 20), sticky="ew")
        button_frame.grid_columnconfigure(2, weight=1)

        self.generate_btn = ctk.CTkButton(
            button_frame,
            text="Generate",
            width=120,
            height=40,
            font=ctk.CTkFont(size=14, weight="bold"),
            command=self._start_merge,
        )
        self.generate_btn.grid(row=0, column=0, padx=(0, 10))

        self.cancel_btn = ctk.CTkButton(
            button_frame,
            text="Cancel",
            width=100,
            height=40,
            fg_color="gray",
            command=self._cancel_merge,
            state="disabled",
        )
        self.cancel_btn.grid(row=0, column=1)

        self.copy_btn = ctk.CTkButton(
            button_frame,
            text="Copy Path",
            width=100,
            height=40,
            fg_color="transparent",
            border_width=1,
            command=self._copy_output_path,
            state="disabled",
        )
        self.copy_btn.grid(row=0, column=3, padx=(10, 0))

        self.open_btn = ctk.CTkButton(
            button_frame,
            text="Open Folder",
            width=100,
            height=40,
            fg_color="transparent",
            border_width=1,
            command=self._open_downloads,
            state="disabled",
        )
        self.open_btn.grid(row=0, column=4, padx=(10, 0))

    def _browse_folder(self):
        """Open folder browser dialog."""
        folder = filedialog.askdirectory(
            title="Select Project Folder",
            initialdir=str(Path.home()),
        )
        if folder:
            self._set_project_path(Path(folder))

    def _set_project_path(self, path: Path):
        """Set the project path and update UI."""
        self.project_path = path
        self.folder_entry.delete(0, "end")
        self.folder_entry.insert(0, str(path))
        self._log(f"Selected: {path}")

    def _log(self, message: str):
        """Add a message to the log."""
        timestamp = datetime.now().strftime("%H:%M:%S")
        self.log_text.insert("end", f"[{timestamp}] {message}\n")
        self.log_text.see("end")

    def _start_merge(self):
        """Start the merge operation based on selected processing mode."""
        # Get project path from entry if not set
        if not self.project_path:
            entry_text = self.folder_entry.get().strip()
            if entry_text:
                self.project_path = Path(entry_text)

        if not self.project_path or not self.project_path.exists():
            self._log("ERROR: Please select a valid project folder")
            return

        if not self.project_path.is_dir():
            self._log("ERROR: Selected path is not a directory")
            return

        # Reset state
        self.is_processing = True
        self.cancel_requested = False
        self.processed_results = []
        self.secret_files = []
        self.skipped_files = []

        # Update UI state
        self.generate_btn.configure(state="disabled")
        self.cancel_btn.configure(state="normal", fg_color="#d9534f")
        self.browse_btn.configure(state="disabled")
        self.copy_btn.configure(state="disabled")
        self.open_btn.configure(state="disabled")
        self.progress_bar.set(0)
        self.progress_label.configure(text="Starting scan...")

        # Clear security tab
        self.security_text.delete("1.0", "end")
        self.security_text.insert("end", "Scanning for secrets...\n")

        processing_mode = self.processing_mode_var.get()
        self._log(f"Starting merge: {self.project_path}")
        self._log(f"Scan mode: {self.mode_var.get()}, Processing: {processing_mode.upper()}")

        if processing_mode == "safe":
            self._start_safe_mode()
        else:
            self._start_turbo_mode()

    def _start_safe_mode(self):
        """Start Safe Mode merge using core.py StreamingMerger."""
        self._safe_mode_cancelled.clear()

        # Determine output path
        downloads = Path.home() / "Downloads"
        downloads.mkdir(exist_ok=True)
        timestamp = datetime.now().strftime("%Y%m%d_%H%M%S")
        self.output_path = downloads / f"{self.project_path.name}_{timestamp}.merged.md"

        def safe_worker():
            try:
                merger = StreamingMerger(
                    project_root=self.project_path,
                    output_path=self.output_path,
                    mode=self.mode_var.get(),
                )

                def progress_callback(current: int, total: int, filename: str):
                    # Update progress via after() to be thread-safe
                    self.after(0, lambda: self._update_safe_progress(current, total, filename))

                def cancel_check() -> bool:
                    return self._safe_mode_cancelled.is_set()

                stats, results = merger.merge(
                    progress_callback=progress_callback,
                    cancel_check=cancel_check,
                )

                if self._safe_mode_cancelled.is_set():
                    self.after(0, self._on_merge_cancelled)
                else:
                    # Convert FileProcessResult to match expected format
                    self.processed_results = results
                    self.secret_files = [r for r in results if r.is_secret]
                    self.skipped_files = [r for r in results if r.is_skipped]
                    self.after(0, lambda: self._on_safe_mode_complete(stats))

            except Exception as e:
                self.after(0, lambda: self._on_merge_error(str(e)))

        self._safe_mode_thread = threading.Thread(target=safe_worker, daemon=True)
        self._safe_mode_thread.start()

    def _update_safe_progress(self, current: int, total: int, filename: str):
        """Update UI with Safe Mode progress."""
        if total > 0:
            pct = current / total
            self.progress_bar.set(pct)
            self.progress_label.configure(text=f"Processing: {filename} ({current}/{total})")

    def _on_safe_mode_complete(self, stats):
        """Handle Safe Mode completion."""
        self.is_processing = False
        self.progress_bar.set(1.0)
        self.progress_label.configure(text="Merge complete!")

        # Update security tab
        self._update_security_tab()

        # Calculate stats
        processed = stats.processed_files
        skipped = stats.skipped_files
        secrets = stats.secret_files
        total_chars = stats.total_chars

        # Count tokens
        token_count = -1
        if self.output_path and self.output_path.exists():
            try:
                output_content = self.output_path.read_text(encoding="utf-8")
                token_count = count_tokens(output_content)
            except Exception:
                pass

        status_text = (
            f"Processed: {processed} files\n"
            f"Skipped: {skipped} files\n"
            f"Secrets found: {secrets} files\n"
            f"Total characters: {total_chars:,}\n"
        )
        if token_count > 0:
            status_text += f"Tokens (GPT-4/Claude): ~{token_count:,}\n"
        status_text += f"Output: {self.output_path.name if self.output_path else 'N/A'}"

        self.status_label.configure(text=status_text)

        if self.output_path:
            self._log(f"Merge complete: {self.output_path}")
        self._log(f"Processed {processed} files, {total_chars:,} chars [SAFE MODE]")
        if token_count > 0:
            self._log(f"Token count: ~{token_count:,}")
        if secrets > 0:
            self._log(f"WARNING: {secrets} files with secrets detected!")

        self._reset_buttons()
        self.copy_btn.configure(state="normal")
        self.open_btn.configure(state="normal")

    def _start_turbo_mode(self):
        """Start Turbo Mode merge using engine.py ProcessingEngine."""
        # Clear engine queues
        while not self.engine.result_queue.empty():
            try:
                self.engine.result_queue.get_nowait()
            except Exception:
                break
        while not self.engine.progress_queue.empty():
            try:
                self.engine.progress_queue.get_nowait()
            except Exception:
                break

        # Start the engine
        self.engine.process_files(
            project_root=self.project_path,
            mode=self.mode_var.get(),
            on_complete=lambda: None,  # Handled in polling
        )

    def _cancel_merge(self):
        """Request cancellation of merge operation."""
        if self.is_processing:
            self.cancel_requested = True

            if self.processing_mode_var.get() == "safe":
                self._safe_mode_cancelled.set()
            else:
                self.engine.cancel()

            self._log("Cancellation requested...")
            self.cancel_btn.configure(state="disabled", text="Cancelling...")

    def _poll_engine(self):
        """Poll the Turbo Mode engine for progress and results."""
        if self.is_processing and self.processing_mode_var.get() == "turbo":
            # Get results
            results = self.engine.get_results()
            for result in results:
                self.processed_results.append(result)
                if result.is_secret:
                    self.secret_files.append(result)
                if result.is_skipped:
                    self.skipped_files.append(result)

            # Get progress
            progress = self.engine.get_progress()
            if progress:
                status, total, completed, filename = progress

                if status == "progress":
                    pct = completed / total if total > 0 else 0
                    self.progress_bar.set(pct)
                    self.progress_label.configure(
                        text=f"Processing: {filename} ({completed}/{total})"
                    )

                elif status == "complete":
                    self._on_turbo_mode_complete()

                elif status == "cancelled":
                    self._on_merge_cancelled()

        # Schedule next poll (100ms for responsive cancellation)
        self.after(100, self._poll_engine)

    def _on_turbo_mode_complete(self):
        """Handle Turbo Mode completion - write output file."""
        self.is_processing = False
        self.progress_bar.set(0.9)
        self.progress_label.configure(text="Writing output file...")

        # Sort results by path for consistent output (Phase 2 of two-phase)
        self.processed_results.sort(key=lambda r: r.relative_path)

        # Write the merged output
        self._write_merged_output()

        # Update security tab
        self._update_security_tab()

        # Update status
        processed = len([r for r in self.processed_results if not r.is_skipped])
        skipped = len(self.skipped_files)
        secrets = len(self.secret_files)
        total_chars = sum(r.char_count for r in self.processed_results if r.content)

        # Count tokens if output exists
        token_count = -1
        if self.output_path and self.output_path.exists():
            try:
                output_content = self.output_path.read_text(encoding="utf-8")
                token_count = count_tokens(output_content)
            except Exception:
                pass

        self.progress_bar.set(1.0)
        self.progress_label.configure(text="Merge complete!")

        status_text = (
            f"Processed: {processed} files\n"
            f"Skipped: {skipped} files\n"
            f"Secrets found: {secrets} files\n"
            f"Total characters: {total_chars:,}\n"
        )
        if token_count > 0:
            status_text += f"Tokens (GPT-4/Claude): ~{token_count:,}\n"
        status_text += f"Output: {self.output_path.name if self.output_path else 'N/A'}"

        self.status_label.configure(text=status_text)

        if self.output_path:
            self._log(f"Merge complete: {self.output_path}")
        self._log(f"Processed {processed} files, {total_chars:,} chars [TURBO MODE]")
        if token_count > 0:
            self._log(f"Token count: ~{token_count:,}")
        if secrets > 0:
            self._log(f"WARNING: {secrets} files with secrets detected!")

        self._reset_buttons()
        self.copy_btn.configure(state="normal")
        self.open_btn.configure(state="normal")

    def _write_merged_output(self):
        """Write the merged output file from Turbo Mode results."""
        # Determine output path
        downloads = Path.home() / "Downloads"
        downloads.mkdir(exist_ok=True)
        timestamp = datetime.now().strftime("%Y%m%d_%H%M%S")
        self.output_path = downloads / f"{self.project_path.name}_{timestamp}.merged.md"

        # Write output (matching Safe Mode format exactly)
        with open(self.output_path, "w", encoding="utf-8") as f:
            # Header
            f.write(f"# {self.project_path.name} - Merged Codebase\n\n")
            f.write(f"> Generated by TurboMerger v{__version__}\n")
            f.write(f"> Timestamp: {datetime.now().strftime('%Y-%m-%d %H:%M:%S')}\n")
            f.write(f"> Mode: {'Smart Code Scan' if self.mode_var.get() == 'smart' else 'Complete Scan'}\n")
            f.write(f"> Source: {self.project_path}\n\n")
            f.write("---\n\n")

            # Table of contents
            f.write("## Table of Contents\n\n")
            for result in self.processed_results:
                if result.content and not result.is_skipped:
                    rel_path = result.relative_path.replace("\\", "/")
                    anchor = rel_path.replace("/", "-").replace(".", "-")
                    f.write(f"- [{rel_path}](#{anchor})\n")
            f.write("\n---\n\n")

            # File contents
            for result in self.processed_results:
                if result.content and not result.is_skipped:
                    rel_path = result.relative_path.replace("\\", "/")

                    # File header with status indicator
                    if result.is_pdf:
                        indicator = "[PDF]"
                    elif result.is_truncated:
                        indicator = "[TRUNCATED]"
                    elif result.is_secret:
                        indicator = "[SECRETS DETECTED]"
                    else:
                        indicator = ""

                    f.write(f"\n## {rel_path} {indicator}\n\n")

                    # Code block
                    lang = result.language or ""
                    f.write(f"```{lang}\n")
                    f.write(result.content)
                    if not result.content.endswith("\n"):
                        f.write("\n")
                    f.write("```\n")

            # Statistics footer
            processed = len([r for r in self.processed_results if not r.is_skipped])
            skipped = len(self.skipped_files)
            total_chars = sum(r.char_count for r in self.processed_results if r.content)

            f.write("\n---\n\n")
            f.write("## Merge Statistics\n\n")
            f.write("| Metric | Value |\n")
            f.write("|--------|-------|\n")
            f.write(f"| Files Processed | {processed} |\n")
            f.write(f"| Files Skipped | {skipped} |\n")
            f.write(f"| Total Characters | {total_chars:,} |\n")
            f.write(f"\n---\n*Generated by TurboMerger v{__version__}*\n")

    def _update_security_tab(self):
        """Update the security tab with scan results."""
        self.security_text.delete("1.0", "end")

        # Check for secrets in both modes (unified interface)
        secret_files_list = []
        content_secrets = []

        for r in self.processed_results:
            if hasattr(r, 'is_secret') and r.is_secret:
                if hasattr(r, 'secret_matches') and r.secret_matches:
                    content_secrets.append(r)
                elif hasattr(r, 'skip_reason') and r.skip_reason in (
                    "SECRET_FILENAME", "ENV_FILE", "SECRET_EXTENSION", "SECRET_CONFIG_FILE"
                ):
                    secret_files_list.append(r)

        for r in self.skipped_files:
            if hasattr(r, 'skip_reason') and r.skip_reason in (
                "SECRET_FILENAME", "ENV_FILE", "SECRET_EXTENSION", "SECRET_CONFIG_FILE"
            ):
                if r not in secret_files_list:
                    secret_files_list.append(r)

        if not secret_files_list and not content_secrets:
            self.security_text.insert("end", "No secrets detected.\n\n")
            self.security_text.insert("end", "All files passed security scan.\n")
        else:
            self.security_text.insert("end", "SECURITY SCAN RESULTS\n")
            self.security_text.insert("end", "=" * 40 + "\n\n")

            # Files skipped due to secret filename
            if secret_files_list:
                self.security_text.insert("end", f"FILES SKIPPED (Secret Filenames): {len(secret_files_list)}\n")
                self.security_text.insert("end", "-" * 40 + "\n")
                for r in secret_files_list:
                    reason = getattr(r, 'skip_reason', 'SECRET')
                    self.security_text.insert("end", f"  - {r.relative_path} [{reason}]\n")
                self.security_text.insert("end", "\n")

            # Files with detected secrets in content
            if content_secrets:
                self.security_text.insert("end", f"FILES WITH SECRETS IN CONTENT: {len(content_secrets)}\n")
                self.security_text.insert("end", "-" * 40 + "\n")
                for r in content_secrets:
                    self.security_text.insert("end", f"  {r.relative_path}:\n")
                    matches = getattr(r, 'secret_matches', [])
                    for match in matches[:5]:  # Limit display
                        self.security_text.insert(
                            "end",
                            f"    Line {match.line_number}: {match.secret_type} ({match.confidence})\n"
                        )
                    if len(matches) > 5:
                        self.security_text.insert(
                            "end",
                            f"    ... and {len(matches) - 5} more\n"
                        )
                self.security_text.insert("end", "\n")

            # Summary
            total_secrets = len(secret_files_list) + len(content_secrets)
            self.security_text.insert("end", f"\nTOTAL: {total_secrets} files with security concerns\n")

    def _on_merge_cancelled(self):
        """Handle merge cancellation."""
        self.is_processing = False
        self.progress_label.configure(text="Merge cancelled")
        self._log("Merge cancelled by user")
        self._reset_buttons()

    def _on_merge_error(self, error_msg: str):
        """Handle merge error."""
        self.is_processing = False
        self.progress_label.configure(text=f"Error: {error_msg}")
        self._log(f"ERROR: {error_msg}")
        if self.processing_mode_var.get() == "turbo":
            self._log("TIP: Try again with Safe Mode for better error handling")
        self._reset_buttons()

    def _reset_buttons(self):
        """Reset button states after operation."""
        self.generate_btn.configure(state="normal")
        self.cancel_btn.configure(state="disabled", fg_color="gray", text="Cancel")
        self.browse_btn.configure(state="normal")

    def _copy_output_path(self):
        """Copy output path to clipboard."""
        if self.output_path:
            self.clipboard_clear()
            self.clipboard_append(str(self.output_path))
            self._log("Output path copied to clipboard")

    def _open_downloads(self):
        """Open Downloads folder in Explorer."""
        downloads = Path.home() / "Downloads"
        if sys.platform == "win32":
            os.startfile(str(downloads))
        else:
            subprocess.run(["xdg-open", str(downloads)])


def main():
    """Launch the TurboMerger GUI."""
    app = TurboMergerApp()
    app.mainloop()


if __name__ == "__main__":
    main()
