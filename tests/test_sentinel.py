"""Tests for sentinel module (secret scanning)."""

from pathlib import Path

import pytest

from turbomerger.sentinel import (
    SECRET_FILES,
    SECRET_PATTERNS,
    SecretMatch,
    SecretResult,
    calculate_entropy,
    check_content_for_secrets,
    check_for_secrets,
    is_high_entropy,
    is_placeholder,
    is_secret_file,
    is_secret_filename,
    scan_file,
)


class TestCalculateEntropy:
    """Tests for entropy calculation."""

    def test_empty_string(self):
        """Empty string has zero entropy."""
        assert calculate_entropy("") == 0.0

    def test_single_char(self):
        """Repeated single char has zero entropy."""
        assert calculate_entropy("aaaa") == 0.0

    def test_high_entropy(self):
        """Random-looking string has high entropy."""
        random_string = "aB3xZ9mK2pL5qR8wE1"
        entropy = calculate_entropy(random_string)
        assert entropy > 3.5  # High entropy

    def test_low_entropy(self):
        """Predictable string has low entropy."""
        low_entropy = "hellohellohello"
        entropy = calculate_entropy(low_entropy)
        assert entropy < 3.0  # Low entropy


class TestIsHighEntropy:
    """Tests for high entropy detection."""

    def test_short_string_not_high_entropy(self):
        """Short strings shouldn't be flagged."""
        assert is_high_entropy("abc123") is False

    def test_long_random_string(self):
        """Long random strings should be flagged."""
        random_key = "aB3xZ9mK2pL5qR8wE1yT6uI4oP7sD0fG"
        assert is_high_entropy(random_key) is True


class TestIsPlaceholder:
    """Tests for placeholder detection."""

    @pytest.mark.parametrize(
        "text",
        [
            "your_api_key_here",
            "your-api-key",
            "placeholder",
            "example_key_123",
            "${API_KEY}",
            "{{SECRET}}",
            "process.env.API_KEY",
            "os.environ.get('KEY')",
        ],
    )
    def test_placeholders(self, text):
        """Known placeholders should be detected."""
        assert is_placeholder(text) is True

    @pytest.mark.parametrize(
        "text",
        [
            "sk-proj-abc123def456ghi789jkl012mno345",
            "AKIAIOSFODNN7ABCDEFG",  # AWS format
            "ghp_0123456789abcdefABCDEF0123456789abcd",  # GitHub PAT
            "sk_live_51ABCdef123456GHI789jklmnop",  # Stripe key
        ],
    )
    def test_real_looking_keys(self, text):
        """Real-looking keys should not be placeholders."""
        assert is_placeholder(text) is False


class TestIsSecretFilename:
    """Tests for secret filename detection."""

    @pytest.mark.parametrize(
        "filename,expected_secret",
        [
            (".env", True),
            (".env.local", True),
            (".env.production", True),
            ("id_rsa", True),
            ("id_ed25519", True),
            ("credentials.json", True),
            ("secrets.json", True),
            ("main.py", False),
            ("config.toml", False),
            ("README.md", False),
        ],
    )
    def test_secret_filenames(self, filename, expected_secret):
        """Secret filenames should be detected."""
        is_secret, reason = is_secret_filename(filename)
        assert is_secret == expected_secret


class TestIsSecretFile:
    """Tests for secret file detection."""

    def test_env_file(self):
        """Env files should be flagged."""
        is_secret, reason = is_secret_file(Path(".env"))
        assert is_secret is True
        assert reason == "SECRET_FILENAME"

    def test_normal_file(self):
        """Normal files should not be flagged."""
        is_secret, reason = is_secret_file(Path("main.py"))
        assert is_secret is False


class TestSecretPatterns:
    """Tests for individual secret detection patterns."""

    def test_aws_access_key(self):
        """AWS access keys should be detected."""
        pattern = SECRET_PATTERNS["AWS Access Key"]
        # AWS keys are prefix (4 chars) + 16 uppercase alphanumeric = 20 total
        assert pattern.search("AKIAIOSFODNN7EXAMPLA")  # 20 chars total
        assert pattern.search("ASIAXYZ1234567890123")  # 20 chars total
        assert not pattern.search("not_an_aws_key")

    def test_openai_new_format_proj(self):
        """OpenAI project keys (new format April 2024) should be detected."""
        pattern = SECRET_PATTERNS["OpenAI API Key"]
        # Project keys can be 148+ chars
        assert pattern.search("sk-proj-abc123def456ghi789jkl012mno345pqr678stu901vwx234yz")
        assert pattern.search("sk-proj-" + "a" * 140)  # Long project key

    def test_openai_admin_key(self):
        """OpenAI admin keys should be detected."""
        pattern = SECRET_PATTERNS["OpenAI API Key"]
        assert pattern.search("sk-admin-xyz789abc123def456ghi")

    def test_openai_legacy_key(self):
        """OpenAI legacy keys should be detected."""
        pattern = SECRET_PATTERNS["OpenAI API Key"]
        assert pattern.search("sk-abcdefghij1234567890abcdefghij12345678")

    def test_stripe_live_key(self):
        """Stripe live keys should be detected."""
        pattern = SECRET_PATTERNS["Stripe Key"]
        # Stripe keys need 24+ chars after prefix
        assert pattern.search("sk_live_51ABCdef123456GHI789jkl012mno")  # 24+ after prefix

    def test_stripe_test_key(self):
        """Stripe test keys should also be detected."""
        pattern = SECRET_PATTERNS["Stripe Key"]
        assert pattern.search("sk_test_51ABCdef123456GHI789jkl012mno")  # 24+ after prefix

    def test_stripe_publishable_key(self):
        """Stripe publishable keys should be detected."""
        pattern = SECRET_PATTERNS["Stripe Key"]
        assert pattern.search("pk_live_51ABCdef123456GHI789jkl012mno")  # 24+ after prefix

    def test_github_pat(self):
        """GitHub PATs should be detected."""
        pattern = SECRET_PATTERNS["GitHub Token"]
        # GitHub tokens need 36+ chars after prefix
        assert pattern.search("ghp_0123456789abcdefABCDEF0123456789abcd")  # 40 chars after prefix
        assert pattern.search("gho_0123456789abcdefABCDEF0123456789abcd")
        assert pattern.search("ghu_0123456789abcdefABCDEF0123456789abcd")

    def test_google_api_key(self):
        """Google API keys should be detected."""
        pattern = SECRET_PATTERNS["Google API Key"]
        # Google API keys are AIza + exactly 35 chars = 39 total
        assert pattern.search("AIzaSyA1234567890abcdefghijklmnopqrstXY")  # 35 chars after AIza

    def test_private_key(self):
        """PEM private keys should be detected."""
        pattern = SECRET_PATTERNS["Private Key"]
        assert pattern.search("-----BEGIN RSA PRIVATE KEY-----")
        assert pattern.search("-----BEGIN PRIVATE KEY-----")
        assert pattern.search("-----BEGIN OPENSSH PRIVATE KEY-----")


class TestCheckContentForSecrets:
    """Tests for content scanning."""

    def test_finds_aws_key(self):
        """AWS key in content should be found."""
        # AWS key: prefix (4) + 16 uppercase alphanumeric = 20 total
        content = "AWS_ACCESS_KEY=AKIAIOSFODNN7EXAMPLA"
        matches = check_content_for_secrets(content)
        assert len(matches) > 0
        assert any(m.secret_type == "AWS Access Key" for m in matches)

    def test_finds_openai_key(self):
        """OpenAI key in content should be found."""
        content = 'api_key = "sk-proj-abc123def456ghi789jkl012mno"'
        matches = check_content_for_secrets(content)
        assert len(matches) > 0
        assert any(m.secret_type == "OpenAI API Key" for m in matches)

    def test_line_number_tracking(self):
        """Line numbers should be tracked correctly."""
        # AWS key with valid length
        content = "line1\nline2\nAKIAIOSFODNN7EXAMPLA\nline4"
        matches = check_content_for_secrets(content)
        assert len(matches) > 0
        assert matches[0].line_number == 3

    def test_skips_placeholders(self):
        """Placeholder values should be skipped."""
        content = 'api_key = "your_api_key_here"'
        matches = check_content_for_secrets(content)
        # Should skip the placeholder
        assert len(matches) == 0 or all(
            m.confidence == "low" or "placeholder" in m.matched_text.lower()
            for m in matches
        )


class TestCheckForSecrets:
    """Tests for full secret check."""

    def test_secret_filename_detected(self):
        """Secret filenames should be detected."""
        result = check_for_secrets("anything", file_path=Path(".env"))
        assert result.has_secrets is True
        assert result.is_secret_file is True

    def test_content_secrets_detected(self):
        """Secrets in content should be detected."""
        # AWS key with valid length (20 chars)
        content = "AWS_KEY=AKIAIOSFODNN7EXAMPLA"
        result = check_for_secrets(content, file_path=Path("config.py"))
        assert result.has_secrets is True
        assert len(result.matches) > 0

    def test_clean_content(self):
        """Clean content should pass."""
        content = "def hello():\n    print('Hello, World!')\n"
        result = check_for_secrets(content, file_path=Path("main.py"))
        assert result.has_secrets is False


class TestScanFile:
    """Tests for file scanning."""

    def test_scan_env_file(self, temp_dir):
        """Env files should be flagged by name."""
        env_file = temp_dir / ".env"
        env_file.write_text("SECRET=value")

        result = scan_file(env_file)
        assert result.has_secrets is True
        assert result.is_secret_file is True

    def test_scan_file_with_secret(self, temp_dir):
        """Files with secrets should be detected."""
        config_file = temp_dir / "config.py"
        # AWS key: prefix (4) + 16 uppercase = 20 total
        config_file.write_text('AWS_KEY = "AKIAIOSFODNN7EXAMPLA"')

        result = scan_file(config_file)
        assert result.has_secrets is True
        assert len(result.matches) > 0

    def test_scan_clean_file(self, temp_dir):
        """Clean files should pass."""
        clean_file = temp_dir / "main.py"
        clean_file.write_text("print('hello')")

        result = scan_file(clean_file)
        assert result.has_secrets is False

    def test_scan_large_file_skipped(self, temp_dir):
        """Large files should be skipped."""
        large_file = temp_dir / "large.txt"
        large_file.write_text("x" * 2_000_000)

        result = scan_file(large_file, max_size_bytes=1_000_000)
        assert result.has_secrets is False  # Skipped, so no secrets


class TestSecretResult:
    """Tests for SecretResult dataclass."""

    def test_default_values(self):
        """Default values should be falsy."""
        result = SecretResult(path=Path("test.py"))
        assert result.has_secrets is False
        assert result.matches == []
        assert result.is_secret_file is False

    def test_with_matches(self):
        """Result with matches should have secrets."""
        match = SecretMatch(
            secret_type="Test",
            line_number=1,
            line_content="test",
            matched_text="xxx",
        )
        result = SecretResult(path=Path("test.py"), has_secrets=True, matches=[match])
        assert result.has_secrets is True
        assert len(result.matches) == 1
