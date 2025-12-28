"""Secret scanning module for TurboMerger.

This module provides detection of API keys, tokens, and other secrets in source code.
Patterns are verified as of December 2025.
"""

import math
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional

# Secret detection patterns - December 2025 verified
SECRET_PATTERNS: dict[str, re.Pattern] = {
    # AWS - Official prefix documentation
    "AWS Access Key": re.compile(
        r"(A3T[A-Z0-9]|AKIA|AGPA|AIDA|AROA|AIPA|ANPA|ANVA|ASIA)[A-Z0-9]{16}"
    ),
    # OpenAI - UPDATED April 2024 format
    # Old format (sk-...T3BlbkFJ...) is OBSOLETE
    # New: sk-proj-*, sk-admin-*, sk-* (legacy)
    # DO NOT restrict to 40-48 chars - project keys can be 148+ chars
    "OpenAI API Key": re.compile(r"sk-(proj-|admin-)?[a-zA-Z0-9_-]{20,}"),
    # Stripe - Including test and publishable keys
    "Stripe Key": re.compile(r"(sk|rk|pk)_(live|test)_[0-9a-zA-Z]{24,}"),
    # GitHub - Personal access tokens (all types)
    "GitHub Token": re.compile(r"gh[pousr]_[A-Za-z0-9_]{36,}"),
    # Google - API keys
    "Google API Key": re.compile(r"AIza[0-9A-Za-z\-_]{35}"),
    # Private Keys (PEM format)
    "Private Key": re.compile(
        r"-----BEGIN (RSA |DSA |EC |OPENSSH |PGP |ENCRYPTED )?PRIVATE KEY-----"
    ),
    # Generic API key patterns (high entropy strings in obvious contexts)
    "Generic API Key": re.compile(
        r'(?i)(api[_-]?key|apikey|secret[_-]?key|auth[_-]?token)["\']?\s*[:=]\s*["\']?([a-zA-Z0-9_\-]{32,})["\']?'
    ),
    # Azure - Storage account keys
    "Azure Storage Key": re.compile(r"DefaultEndpointsProtocol=https;AccountName=[^;]+;AccountKey=[^;]+"),
    # Slack - Bot and webhook tokens
    "Slack Token": re.compile(r"xox[baprs]-[0-9]{10,13}-[0-9]{10,13}-[a-zA-Z0-9]{24}"),
    # Twilio - Account SID and auth token
    "Twilio Key": re.compile(r"SK[0-9a-fA-F]{32}"),
    # SendGrid - API keys
    "SendGrid Key": re.compile(r"SG\.[a-zA-Z0-9_-]{22}\.[a-zA-Z0-9_-]{43}"),
    # Mailchimp - API keys
    "Mailchimp Key": re.compile(r"[0-9a-f]{32}-us[0-9]{1,2}"),
    # JWT tokens (with payload)
    "JWT Token": re.compile(r"eyJ[a-zA-Z0-9_-]*\.eyJ[a-zA-Z0-9_-]*\.[a-zA-Z0-9_-]*"),
    # Database connection strings
    "Database URL": re.compile(
        r"(?i)(postgres|mysql|mongodb|redis)://[^\s\"'<>]+:[^\s\"'<>]+@[^\s\"'<>]+"
    ),
}

# Files that should ALWAYS be skipped (contain secrets by definition)
SECRET_FILES: set[str] = {
    ".env",
    ".env.local",
    ".env.production",
    ".env.development",
    ".env.staging",
    ".env.test",
    "id_rsa",
    "id_dsa",
    "id_ed25519",
    "id_ecdsa",
    ".pem",
    ".key",
    ".p12",
    ".pfx",
    ".keystore",
    ".jks",
    "secrets.json",
    "credentials.json",
    "service-account.json",
    "gcp-credentials.json",
    ".htpasswd",
    ".netrc",
    ".npmrc",
    ".pypirc",
    "token.json",
    "oauth_token",
    "access_token",
    ".boto",
    "aws_credentials",
}

# Extensions that often contain secrets
SECRET_EXTENSIONS: set[str] = {
    ".pem",
    ".key",
    ".p12",
    ".pfx",
    ".keystore",
    ".jks",
    ".crt",
    ".cer",
}

# Entropy thresholds for high-entropy string detection
ENTROPY_THRESHOLD_BASE64 = 4.5  # bits/char
ENTROPY_THRESHOLD_HEX = 3.0  # bits/char


@dataclass
class SecretMatch:
    """Represents a detected secret in content."""

    secret_type: str
    line_number: int
    line_content: str
    matched_text: str
    confidence: str = "high"  # high, medium, low


@dataclass
class SecretResult:
    """Result of scanning a file for secrets."""

    path: Path
    has_secrets: bool = False
    matches: list[SecretMatch] = field(default_factory=list)
    is_secret_file: bool = False
    skip_reason: str = ""


def calculate_entropy(data: str) -> float:
    """
    Calculate Shannon entropy of a string.

    Higher entropy indicates more randomness (likely a secret).

    Args:
        data: String to calculate entropy for.

    Returns:
        Entropy in bits per character.
    """
    if not data:
        return 0.0

    entropy = 0.0
    length = len(data)

    # Count character frequencies
    freq: dict[str, int] = {}
    for char in data:
        freq[char] = freq.get(char, 0) + 1

    # Calculate entropy
    for count in freq.values():
        probability = count / length
        entropy -= probability * math.log2(probability)

    return entropy


def is_high_entropy(text: str, threshold: float = ENTROPY_THRESHOLD_BASE64) -> bool:
    """
    Check if a string has high entropy (likely a secret).

    Args:
        text: String to check.
        threshold: Entropy threshold.

    Returns:
        True if entropy exceeds threshold.
    """
    if len(text) < 16:
        return False
    return calculate_entropy(text) > threshold


def is_secret_filename(filename: str) -> tuple[bool, str]:
    """
    Check if a filename indicates a secret file.

    Args:
        filename: Name of the file (not full path).

    Returns:
        Tuple of (is_secret, reason).
    """
    name_lower = filename.lower()

    # Check exact matches
    if name_lower in SECRET_FILES:
        return True, "SECRET_FILENAME"

    # Check if starts with .env
    if name_lower.startswith(".env"):
        return True, "ENV_FILE"

    # Check extensions
    for ext in SECRET_EXTENSIONS:
        if name_lower.endswith(ext):
            return True, "SECRET_EXTENSION"

    # Check for common secret file patterns
    secret_patterns = ["credentials", "secrets", "private", "token", "apikey", "password"]
    for pattern in secret_patterns:
        if pattern in name_lower and any(
            name_lower.endswith(ext) for ext in [".json", ".yaml", ".yml", ".xml", ".ini", ".conf"]
        ):
            return True, "SECRET_CONFIG_FILE"

    return False, ""


def is_secret_file(file_path: Path) -> tuple[bool, str]:
    """
    Check if a file path indicates a secret file.

    Args:
        file_path: Path to check.

    Returns:
        Tuple of (is_secret, reason).
    """
    return is_secret_filename(file_path.name)


def check_content_for_secrets(
    content: str,
    file_path: Optional[Path] = None,
) -> list[SecretMatch]:
    """
    Scan content for secrets using regex patterns.

    Args:
        content: Text content to scan.
        file_path: Optional file path for context.

    Returns:
        List of SecretMatch objects for detected secrets.
    """
    matches: list[SecretMatch] = []
    lines = content.split("\n")

    for line_num, line in enumerate(lines, 1):
        # Skip very long lines (likely minified/binary)
        if len(line) > 1000:
            continue

        # Skip comment lines (reduce false positives)
        stripped = line.strip()
        if stripped.startswith(("#", "//", "/*", "*", "<!--")):
            # Still check for actual secrets in comments
            pass

        for secret_type, pattern in SECRET_PATTERNS.items():
            for match in pattern.finditer(line):
                matched_text = match.group(0)

                # Skip if it's clearly a placeholder
                if is_placeholder(matched_text):
                    continue

                # Truncate matched text for display (security)
                display_text = matched_text[:20] + "..." if len(matched_text) > 20 else matched_text

                # Determine confidence
                confidence = "high"
                if secret_type == "Generic API Key":
                    confidence = "medium"
                elif "example" in line.lower() or "test" in line.lower():
                    confidence = "low"

                matches.append(
                    SecretMatch(
                        secret_type=secret_type,
                        line_number=line_num,
                        line_content=line[:100] + "..." if len(line) > 100 else line,
                        matched_text=display_text,
                        confidence=confidence,
                    )
                )

    return matches


def is_placeholder(text: str) -> bool:
    """
    Check if a string is likely a placeholder, not a real secret.

    Args:
        text: String to check.

    Returns:
        True if it looks like a placeholder.
    """
    lower = text.lower()

    # Environment variable references
    env_patterns = ["${", "{{", "ENV[", "process.env", "os.environ", "getenv"]
    if any(p in lower for p in env_patterns):
        return True

    # Obvious placeholder words at the START of the key value part
    # (not in the middle of what looks like a real key)
    obvious_placeholders = [
        "your_api_key",
        "your-api-key",
        "your_key",
        "your-key",
        "placeholder",
        "replace_me",
        "insert_here",
        "dummy_key",
        "fake_key",
        "sample_key",
        "demo_key",
        "<your",
    ]
    if any(lower.startswith(p) or lower == p for p in obvious_placeholders):
        return True

    # Check for test/example only if it's clearly a placeholder context
    # (e.g., "test_key_here" but NOT "sk_test_51ABC" which is a real Stripe test key format)
    if lower.startswith("test_") and "test_" in lower[5:]:
        return True
    if lower.startswith("example_"):
        return True

    return False


def check_for_secrets(
    content: str,
    file_path: Optional[Path] = None,
) -> SecretResult:
    """
    Full secret check for a file's content.

    Args:
        content: Text content to scan.
        file_path: Path to the file.

    Returns:
        SecretResult with all findings.
    """
    result = SecretResult(path=file_path or Path("unknown"))

    # Check if filename indicates secrets
    if file_path:
        is_secret, reason = is_secret_file(file_path)
        if is_secret:
            result.is_secret_file = True
            result.has_secrets = True
            result.skip_reason = reason
            return result

    # Scan content for secrets
    matches = check_content_for_secrets(content, file_path)
    if matches:
        result.has_secrets = True
        result.matches = matches

    return result


def scan_file(file_path: Path, max_size_bytes: int = 1024 * 1024) -> SecretResult:
    """
    Scan a file for secrets.

    Args:
        file_path: Path to the file to scan.
        max_size_bytes: Maximum file size to scan (default 1MB).

    Returns:
        SecretResult with findings.
    """
    result = SecretResult(path=file_path)

    # Check filename first
    is_secret, reason = is_secret_file(file_path)
    if is_secret:
        result.is_secret_file = True
        result.has_secrets = True
        result.skip_reason = reason
        return result

    # Check file size
    try:
        size = file_path.stat().st_size
        if size > max_size_bytes:
            return result  # Skip large files
    except OSError:
        return result

    # Read and scan content
    try:
        with open(file_path, "r", encoding="utf-8", errors="ignore") as f:
            content = f.read()
        return check_for_secrets(content, file_path)
    except Exception:
        return result
