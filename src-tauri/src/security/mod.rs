//! Security module for path validation and sanitization
//! SECURITY FIX: Handle-based file operations to prevent TOCTOU

use std::path::{Path, PathBuf};
use std::fs::{self, File, OpenOptions};
use std::io;

#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

use regex::Regex;
use lazy_static::lazy_static;

/// Windows file attribute for reparse points (junctions/symlinks)
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// Windows flag to open reparse point itself, not follow it
#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x00200000;

/// Blocked system paths that should never be accessed (Windows)
const BLOCKED_PREFIXES: &[&str] = &[
    "windows",
    "program files",
    "program files (x86)",
    "programdata",
    "$recycle.bin",
    "system volume information",
];

/// Sensitive file patterns to exclude from output
const SENSITIVE_PATTERNS: &[&str] = &[
    ".env", ".env.local", ".env.production",
    "secrets.json", "credentials",
    ".aws", ".ssh", ".gnupg",
    "id_rsa", "id_ed25519", "id_ecdsa",
    ".pem", ".key", ".pfx", ".p12",
    "password", "api_key", "apikey",
    "secret_key", "private_key", "auth_token",
];

#[derive(Debug)]
pub enum SecurityError {
    ValidationFailed(String),
    SystemPathBlocked,
    SensitiveFileBlocked,
    PathEscaped,
    SymlinkAttack,
    ReparsePointDetected,
    IoError(io::Error),
}

impl std::fmt::Display for SecurityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecurityError::ValidationFailed(msg) => write!(f, "Path validation failed: {}", msg),
            SecurityError::SystemPathBlocked => write!(f, "Access denied: system path"),
            SecurityError::SensitiveFileBlocked => write!(f, "Access denied: sensitive file"),
            SecurityError::PathEscaped => write!(f, "Path escaped root directory"),
            SecurityError::SymlinkAttack => write!(f, "Symlink attack detected"),
            SecurityError::ReparsePointDetected => write!(f, "Junction point or reparse point detected"),
            SecurityError::IoError(e) => write!(f, "IO error: {}", e),
        }
    }
}

impl std::error::Error for SecurityError {}

impl From<io::Error> for SecurityError {
    fn from(e: io::Error) -> Self {
        SecurityError::IoError(e)
    }
}

/// SECURITY FIX: Check if any path component is a reparse point (junction/symlink)
/// This prevents junction point attacks that bypass blocklist validation
#[cfg(windows)]
pub fn has_reparse_point_in_path(path: &Path) -> Result<bool, SecurityError> {
    for ancestor in path.ancestors() {
        if ancestor.as_os_str().is_empty() {
            continue;
        }

        // Use symlink_metadata to not follow the link
        match fs::symlink_metadata(ancestor) {
            Ok(meta) => {
                let attrs = meta.file_attributes();
                if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    return Ok(true);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(SecurityError::IoError(e)),
        }
    }
    Ok(false)
}

#[cfg(not(windows))]
pub fn has_reparse_point_in_path(path: &Path) -> Result<bool, SecurityError> {
    // On non-Windows, check for symlinks in path
    for ancestor in path.ancestors() {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        match fs::symlink_metadata(ancestor) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Ok(true);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(SecurityError::IoError(e)),
        }
    }
    Ok(false)
}

/// SECURITY FIX: Normalize path to long name format (defeats 8.3 short name bypass)
#[cfg(windows)]
pub fn normalize_to_long_path(path: &Path) -> Result<PathBuf, SecurityError> {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    // Convert to wide string
    let wide: Vec<u16> = path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // Get required buffer size
    let len = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetLongPathNameW(
            wide.as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };

    if len == 0 {
        // Path doesn't exist or error - return original
        return Ok(path.to_path_buf());
    }

    let mut buffer: Vec<u16> = vec![0; len as usize];
    let result = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetLongPathNameW(
            wide.as_ptr(),
            buffer.as_mut_ptr(),
            len,
        )
    };

    if result == 0 {
        return Ok(path.to_path_buf());
    }

    // Trim null terminator
    buffer.truncate(result as usize);
    Ok(PathBuf::from(OsString::from_wide(&buffer)))
}

#[cfg(not(windows))]
pub fn normalize_to_long_path(path: &Path) -> Result<PathBuf, SecurityError> {
    Ok(path.to_path_buf())
}

/// SECURITY FIX: Unicode NFKC normalization before validation
pub fn normalize_unicode(input: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    input.nfkc().collect()
}

/// SECURITY FIX: Validates path with reparse point and junction detection
pub fn validate_and_canonicalize(path: &str) -> Result<PathBuf, SecurityError> {
    // Step 1: Unicode normalization (prevents fullwidth character bypass)
    let normalized_input = normalize_unicode(path);
    let path = PathBuf::from(&normalized_input);

    // Step 2: Check for reparse points BEFORE canonicalization
    // This catches junction attacks before they can redirect us
    if has_reparse_point_in_path(&path)? {
        return Err(SecurityError::ReparsePointDetected);
    }

    // Step 3: Canonicalize
    let canonical = fs::canonicalize(&path)
        .map_err(|e| SecurityError::ValidationFailed(e.to_string()))?;

    // Step 4: Check for reparse points in canonicalized path too
    // (defense in depth)
    if has_reparse_point_in_path(&canonical)? {
        return Err(SecurityError::ReparsePointDetected);
    }

    // Step 5: Normalize to long path name (defeats 8.3 short name bypass)
    let long_path = normalize_to_long_path(&canonical)?;

    // Step 6: Block sensitive system paths
    for component in long_path.components() {
        let comp_str = component.as_os_str().to_string_lossy().to_lowercase();
        for prefix in BLOCKED_PREFIXES {
            if comp_str == *prefix {
                return Err(SecurityError::SystemPathBlocked);
            }
        }
    }

    Ok(long_path)
}

/// SECURITY FIX: Safe file open that prevents symlink following
#[cfg(windows)]
pub fn safe_open_file(path: &Path) -> Result<File, SecurityError> {
    // Check for reparse points first
    if has_reparse_point_in_path(path)? {
        return Err(SecurityError::ReparsePointDetected);
    }

    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)  // Don't follow reparse points
        .open(path)
        .map_err(SecurityError::IoError)
}

#[cfg(not(windows))]
pub fn safe_open_file(path: &Path) -> Result<File, SecurityError> {
    // Check it's not a symlink
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(SecurityError::SymlinkAttack);
    }

    File::open(path).map_err(SecurityError::IoError)
}

/// Validates a path is within a root directory (symlink-safe)
pub fn is_within_root(path: &Path, root: &Path) -> Result<bool, SecurityError> {
    // Check for reparse points in both paths
    if has_reparse_point_in_path(path)? || has_reparse_point_in_path(root)? {
        return Err(SecurityError::ReparsePointDetected);
    }

    let canonical_path = fs::canonicalize(path)
        .map_err(|_| SecurityError::ValidationFailed("Cannot canonicalize path".into()))?;
    let canonical_root = fs::canonicalize(root)
        .map_err(|_| SecurityError::ValidationFailed("Cannot canonicalize root".into()))?;

    Ok(canonical_path.starts_with(&canonical_root))
}

/// Sanitizes a filename by removing dangerous characters
pub fn sanitize_filename(name: &str) -> String {
    // SECURITY FIX: Also block Windows reserved names
    let reserved_names = [
        "CON", "PRN", "AUX", "NUL",
        "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9",
        "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];

    let upper = name.to_uppercase();
    let base_name = upper.split('.').next().unwrap_or("");

    if reserved_names.contains(&base_name) {
        return format!("_{}", name);
    }

    name.chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
        .take(64)
        .collect::<String>()
        .trim_matches('.')
        .to_string()
}

/// Checks if a file appears to be sensitive
pub fn is_sensitive_file(path: &Path) -> bool {
    let path_str = path.to_string_lossy().to_lowercase();
    SENSITIVE_PATTERNS.iter().any(|pattern| path_str.contains(pattern))
}

/// Binary file signatures (magic bytes)
const BINARY_SIGNATURES: &[&[u8]] = &[
    b"\x7FELF",           // ELF executables
    b"MZ",                // PE/EXE files
    b"\x89PNG",           // PNG images
    b"\xFF\xD8\xFF",      // JPEG images
    b"%PDF",              // PDF documents
    b"PK\x03\x04",        // ZIP/DOCX/JAR archives
    b"\x1F\x8B",          // GZIP compression
    b"BZ",                // BZIP2 compression
    b"\xCA\xFE\xBA\xBE",  // Java class files
    b"\xCF\xFA\xED\xFE",  // Mach-O executables (32-bit)
    b"\xFE\xED\xFA\xCF",  // Mach-O executables (64-bit)
    b"\xFE\xED\xFA\xCE",  // Mach-O universal binary
    b"RIFF",              // WAV/AVI files
    b"GIF8",              // GIF images
    b"\x00\x00\x01\x00",  // ICO files
    b"SQLite format 3",   // SQLite databases
    b"\xFD7zXZ\x00",      // XZ compression
    b"Rar!\x1A\x07",      // RAR archives
];

/// Enhanced binary detection with magic bytes
pub fn is_binary_content(data: &[u8]) -> bool {
    if data.is_empty() {
        return false;
    }

    // Check magic bytes first (fast path)
    for sig in BINARY_SIGNATURES {
        if data.starts_with(sig) {
            return true;
        }
    }

    // Heuristic: >5% null bytes indicates binary
    let null_count = data.iter().filter(|&&b| b == 0).count();
    if null_count > data.len() / 20 {
        return true;
    }

    // Heuristic: <85% printable characters indicates binary
    let printable = data.iter().filter(|&&b| {
        b == 9 || b == 10 || b == 13 || (b >= 32 && b < 127) || b >= 128
    }).count();

    printable < data.len() * 85 / 100
}

// =============================================================================
// SECRET DETECTION
// =============================================================================

lazy_static! {
    /// Comprehensive secret detection patterns
    /// Based on: https://github.com/mazen160/secrets-patterns-db
    static ref SECRET_PATTERNS: Vec<(&'static str, Regex)> = vec![
        // AWS
        ("AWS Access Key ID", Regex::new(r"(?i)(AKIA|ASIA|AIDA|AROA)[A-Z0-7]{16}").unwrap()),
        ("AWS Secret Key", Regex::new(r#"(?i)aws[_\-\.]?secret[_\-\.]?access[_\-\.]?key\s*[:=]\s*['"]?([A-Za-z0-9/+=]{40})['"]?"#).unwrap()),

        // GitHub
        ("GitHub PAT", Regex::new(r"ghp_[A-Za-z0-9]{36,}").unwrap()),
        ("GitHub OAuth", Regex::new(r"gho_[A-Za-z0-9]{36,}").unwrap()),
        ("GitHub App", Regex::new(r"(ghu|ghs|ghr)_[A-Za-z0-9]{36,}").unwrap()),
        ("GitHub Fine-grained PAT", Regex::new(r"github_pat_[A-Za-z0-9_]{22,}").unwrap()),

        // Stripe
        ("Stripe Live Key", Regex::new(r"sk_live_[A-Za-z0-9]{24,}").unwrap()),
        ("Stripe Test Key", Regex::new(r"sk_test_[A-Za-z0-9]{24,}").unwrap()),
        ("Stripe Restricted Key", Regex::new(r"rk_(live|test)_[A-Za-z0-9]{24,}").unwrap()),

        // Slack
        ("Slack Token", Regex::new(r"xox[baprs]-[0-9A-Za-z\-]{10,}").unwrap()),
        ("Slack Webhook", Regex::new(r"https://hooks\.slack\.com/services/T[A-Z0-9]+/B[A-Z0-9]+/[A-Za-z0-9]+").unwrap()),

        // Google
        ("Google API Key", Regex::new(r"AIza[0-9A-Za-z\-_]{35}").unwrap()),
        ("Google OAuth", Regex::new(r"[0-9]+-[a-z0-9]+\.apps\.googleusercontent\.com").unwrap()),

        // Azure
        ("Azure Storage Key", Regex::new(r"DefaultEndpointsProtocol=https;AccountName=[^;]+;AccountKey=[A-Za-z0-9+/=]{88};").unwrap()),

        // JWT
        ("JWT Token", Regex::new(r"eyJ[A-Za-z0-9_-]*\.eyJ[A-Za-z0-9_-]*\.[A-Za-z0-9_-]*").unwrap()),

        // Private Keys
        ("RSA Private Key", Regex::new(r"-----BEGIN RSA PRIVATE KEY-----").unwrap()),
        ("SSH Private Key", Regex::new(r"-----BEGIN OPENSSH PRIVATE KEY-----").unwrap()),
        ("EC Private Key", Regex::new(r"-----BEGIN EC PRIVATE KEY-----").unwrap()),
        ("PGP Private Key", Regex::new(r"-----BEGIN PGP PRIVATE KEY BLOCK-----").unwrap()),

        // Database
        ("PostgreSQL URL", Regex::new(r"postgres://[^:]+:[^@]+@[^/]+/[^\s]+").unwrap()),
        ("MySQL URL", Regex::new(r"mysql://[^:]+:[^@]+@[^/]+/[^\s]+").unwrap()),
        ("MongoDB URL", Regex::new(r"mongodb(\+srv)?://[^:]+:[^@]+@[^\s]+").unwrap()),

        // Generic
        ("Generic API Key", Regex::new(r#"(?i)(api[_\-\.]?key|apikey)\s*[:=]\s*['\"]?([A-Za-z0-9\-_]{20,})['\"]?"#).unwrap()),
        ("Generic Secret", Regex::new(r#"(?i)(secret|password|passwd|pwd)\s*[:=]\s*['\"]?([^\s'"]{8,})['\"]?"#).unwrap()),
        ("Bearer Token", Regex::new(r"(?i)bearer\s+[A-Za-z0-9\-_\.]+").unwrap()),

        // NPM
        ("npm Token", Regex::new(r"npm_[A-Za-z0-9]{36}").unwrap()),

        // Discord
        ("Discord Token", Regex::new(r"[MN][A-Za-z\d]{23,}\.[\w-]{6}\.[\w-]{27}").unwrap()),
        ("Discord Webhook", Regex::new(r"https://discord(app)?\.com/api/webhooks/[0-9]+/[A-Za-z0-9_-]+").unwrap()),

        // Twilio
        ("Twilio API Key", Regex::new(r"SK[a-f0-9]{32}").unwrap()),

        // SendGrid
        ("SendGrid API Key", Regex::new(r"SG\.[A-Za-z0-9_-]{22}\.[A-Za-z0-9_-]{43}").unwrap()),

        // Mailchimp
        ("Mailchimp API Key", Regex::new(r"[a-f0-9]{32}-us[0-9]{1,2}").unwrap()),
    ];
}

/// Check content for secrets and return redacted version
pub fn redact_secrets(content: &str) -> (String, Vec<String>) {
    let mut redacted = content.to_string();
    let mut found_secrets = Vec::new();

    for (name, pattern) in SECRET_PATTERNS.iter() {
        if let Some(caps) = pattern.find(&redacted) {
            let preview_end = caps.end().min(caps.start() + 20);
            found_secrets.push(format!("{}: {}...", name, &redacted[caps.start()..preview_end]));
            redacted = pattern.replace_all(&redacted, "[REDACTED]").to_string();
        }
    }

    (redacted, found_secrets)
}

/// Calculate Shannon entropy for high-entropy string detection
pub fn calculate_entropy(data: &str) -> f64 {
    if data.is_empty() {
        return 0.0;
    }

    let mut freq = [0u32; 256];
    for byte in data.bytes() {
        freq[byte as usize] += 1;
    }

    let len = data.len() as f64;
    freq.iter()
        .filter(|&&count| count > 0)
        .map(|&count| {
            let p = count as f64 / len;
            -p * p.log2()
        })
        .sum()
}

/// Detect high-entropy strings that might be secrets
pub fn detect_high_entropy_secrets(content: &str) -> Vec<String> {
    let mut suspicious = Vec::new();

    // Look for base64-like strings with high entropy
    let base64_pattern = Regex::new(r"[A-Za-z0-9+/=]{20,}").unwrap();

    for caps in base64_pattern.find_iter(content) {
        let matched = caps.as_str();
        let entropy = calculate_entropy(matched);

        // Threshold: >4.5 bits per character is suspicious for base64
        if entropy > 4.5 {
            let preview_len = matched.len().min(20);
            suspicious.push(format!("High-entropy string (entropy={:.2}): {}...",
                entropy, &matched[..preview_len]));
        }
    }

    suspicious
}
