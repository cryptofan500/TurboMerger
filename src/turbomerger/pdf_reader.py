"""PDF text extraction utilities."""

from pathlib import Path
from typing import Optional


def extract_pdf_text(file_path: Path, max_pages: int = 50) -> Optional[str]:
    """
    Extract text content from a PDF file.

    Args:
        file_path: Path to the PDF file.
        max_pages: Maximum number of pages to extract.

    Returns:
        Extracted text content, or None if extraction fails.
    """
    try:
        from pypdf import PdfReader

        reader = PdfReader(str(file_path))
        pages = reader.pages[:max_pages]

        text_parts = []
        for i, page in enumerate(pages, 1):
            try:
                page_text = page.extract_text()
                if page_text and page_text.strip():
                    text_parts.append(f"--- Page {i} ---\n{page_text}")
            except Exception:
                text_parts.append(f"--- Page {i} ---\n[Error extracting page]")

        if not text_parts:
            return None

        result = "\n\n".join(text_parts)

        # Add truncation notice if we hit the page limit
        if len(reader.pages) > max_pages:
            result += f"\n\n[... Truncated: showing {max_pages} of {len(reader.pages)} pages ...]"

        return result

    except ImportError:
        return "[PDF extraction unavailable - pypdf not installed]"
    except Exception as e:
        return f"[PDF extraction error: {e}]"


def is_pdf_readable(file_path: Path) -> bool:
    """
    Check if a PDF file can be read.

    Args:
        file_path: Path to the PDF file.

    Returns:
        True if PDF is readable, False otherwise.
    """
    try:
        from pypdf import PdfReader

        reader = PdfReader(str(file_path))
        # Try to access page count
        _ = len(reader.pages)
        return True
    except Exception:
        return False
