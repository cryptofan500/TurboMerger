//! Security module: path validation, binary detection, and secret redaction.
//!
//! v7.2: sensitive-file matching moved from whole-path substrings (which silently
//! dropped legit files like `password_reset.py`) to filename-based rules that
//! return a human-readable reason; secret redaction is now actually wired into
//! the merge path (it was dead code in v6/v7.1) with an entropy gate + stopwords
//! to keep false positives tolerable.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

/// Windows file attribute for reparse points (junctions/symlinks)
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// Top-level system directories that must never be scanned. Checked against the
/// FIRST component under the drive root only — a repo folder named `windows`
/// somewhere deeper is fine.
const BLOCKED_ROOT_DIRS: &[&str] = &[
    "windows",
    "program files",
    "program files (x86)",
    "programdata",
    "$recycle.bin",
    "system volume information",
];

#[derive(Debug)]
pub enum SecurityError {
    ValidationFailed(String),
    SystemPathBlocked,
    ReparsePointDetected,
    IoError(io::Error),
}

impl std::fmt::Display for SecurityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecurityError::ValidationFailed(msg) => write!(f, "Path validation failed: {}", msg),
            SecurityError::SystemPathBlocked => write!(f, "Access denied: system path"),
            SecurityError::ReparsePointDetected => {
                write!(f, "Junction point or reparse point detected")
            }
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

/// Check if any path component is a reparse point (junction/symlink).
/// Used once on the scan root; per-file checks are handled by the walker.
#[cfg(windows)]
pub fn has_reparse_point_in_path(path: &Path) -> Result<bool, SecurityError> {
    use std::os::windows::fs::MetadataExt;
    for ancestor in path.ancestors() {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        match fs::symlink_metadata(ancestor) {
            Ok(meta) => {
                if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
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

/// Normalize path to long name format (defeats 8.3 short name bypass)
#[cfg(windows)]
pub fn normalize_to_long_path(path: &Path) -> Result<PathBuf, SecurityError> {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let len = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetLongPathNameW(
            wide.as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };

    if len == 0 {
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

    buffer.truncate(result as usize);
    Ok(PathBuf::from(OsString::from_wide(&buffer)))
}

#[cfg(not(windows))]
pub fn normalize_to_long_path(path: &Path) -> Result<PathBuf, SecurityError> {
    Ok(path.to_path_buf())
}

/// Unicode NFKC normalization before validation
pub fn normalize_unicode(input: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    input.nfkc().collect()
}

/// Validates a scan root: unicode-normalized, reparse-point-free, canonical,
/// long-named, and not a Windows system root.
pub fn validate_and_canonicalize(path: &str) -> Result<PathBuf, SecurityError> {
    let normalized_input = normalize_unicode(path);
    let path = PathBuf::from(&normalized_input);

    if has_reparse_point_in_path(&path)? {
        return Err(SecurityError::ReparsePointDetected);
    }

    let canonical =
        fs::canonicalize(&path).map_err(|e| SecurityError::ValidationFailed(e.to_string()))?;

    if has_reparse_point_in_path(&canonical)? {
        return Err(SecurityError::ReparsePointDetected);
    }

    let long_path = normalize_to_long_path(&canonical)?;

    // Block scanning system roots (C:\Windows, C:\Program Files, …) — first
    // normal component under the drive root only.
    if let Some(Component::Normal(first)) = long_path
        .components()
        .find(|c| matches!(c, Component::Normal(_)))
    {
        let s = first.to_string_lossy().to_lowercase();
        if BLOCKED_ROOT_DIRS.contains(&s.as_str()) {
            return Err(SecurityError::SystemPathBlocked);
        }
    }

    Ok(long_path)
}

/// Sanitizes a filename by removing dangerous characters (filesystem name only —
/// display titles should use the raw folder name).
pub fn sanitize_filename(name: &str) -> String {
    let reserved_names = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];

    let upper = name.to_uppercase();
    let base_name = upper.split('.').next().unwrap_or("");

    if reserved_names.contains(&base_name) {
        return format!("_{}", name);
    }

    name.chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == '.' || *c == ' ')
        .take(64)
        .collect::<String>()
        .trim_matches(['.', ' '])
        .to_string()
}

/// Filename-based sensitive-file detection. Returns a reason string when the
/// file should be excluded, `None` otherwise. Deliberately narrow: matching the
/// whole path by substring (pre-v7.2) silently dropped ordinary source files
/// like `password_reset.py`, `useApiKey.ts`, and `config.environments.ts`.
pub fn sensitive_reason(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();

    // .env family — templates are explicitly safe
    if name == ".env"
        || (name.starts_with(".env.")
            && !matches!(
                name.as_str(),
                ".env.example" | ".env.sample" | ".env.template"
            ))
    {
        return Some("env file (may hold credentials)");
    }

    // Key/certificate material by (last) extension
    if let Some(ext) = Path::new(&name).extension().and_then(|e| e.to_str()) {
        if matches!(ext, "pem" | "pfx" | "p12" | "jks" | "keystore" | "key") {
            return Some("key/certificate material");
        }
    }

    // SSH identity files
    if name.starts_with("id_rsa")
        || name.starts_with("id_ed25519")
        || name.starts_with("id_ecdsa")
        || name.starts_with("id_dsa")
    {
        return Some("SSH private key");
    }

    // Well-known credential stores
    if matches!(
        name.as_str(),
        "credentials.json"
            | "secrets.json"
            | "secrets.yaml"
            | "secrets.yml"
            | "service-account.json"
            | ".netrc"
            | "_netrc"
            | ".pgpass"
            | ".htpasswd"
            | "htpasswd"
            | ".npmrc"
    ) {
        return Some("credential store");
    }

    // Credential/secret/password DATA files (e.g. the `<NAME>_CREDENTIALS_<UTC>.md`
    // convention, `vault.txt`, `passwords.csv`). Only DATA/DOC extensions match, so
    // ordinary SOURCE files (`password_reset.py`, `useApiKey.ts`, `credentials_form.tsx`)
    // are NOT swept up. This is the primary defense when merging credential-heavy trees:
    // the whole file is excluded (and reported), never partially redacted. For DATA
    // files a plain substring match is used deliberately — a false positive (a doc
    // excluded but listed in the report, includable-anyway) is far cheaper than a
    // false negative that leaks a credential dump to a web LLM.
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) => (s, e),
        None => (name.as_str(), ""),
    };
    const DATA_EXTS: &[&str] = &[
        "md", "markdown", "txt", "text", "csv", "tsv", "json", "yaml", "yml", "toml", "ini",
        "conf", "cfg", "env", "log", "xlsx", "rtf",
    ];
    if DATA_EXTS.contains(&ext) {
        for marker in [
            "credential",
            "password",
            "passwd",
            "secret",
            "vault",
            "apikey",
            "api_key",
            "api-key",
        ] {
            if stem.contains(marker) {
                return Some("credential/secret data file");
            }
        }
    }

    None
}

/// Binary file signatures (magic bytes)
const BINARY_SIGNATURES: &[&[u8]] = &[
    b"\x7FELF",          // ELF executables
    b"MZ",               // PE/EXE files
    b"\x89PNG",          // PNG images
    b"\xFF\xD8\xFF",     // JPEG images
    b"%PDF",             // PDF documents
    b"PK\x03\x04",       // ZIP/DOCX/JAR archives
    b"\x1F\x8B",         // GZIP compression
    b"BZ",               // BZIP2 compression
    b"\xCA\xFE\xBA\xBE", // Java class files
    b"\xCF\xFA\xED\xFE", // Mach-O executables (32-bit)
    b"\xFE\xED\xFA\xCF", // Mach-O executables (64-bit)
    b"\xFE\xED\xFA\xCE", // Mach-O universal binary
    b"RIFF",             // WAV/AVI files
    b"GIF8",             // GIF images
    b"\x00\x00\x01\x00", // ICO files
    b"SQLite format 3",  // SQLite databases
    b"\xFD7zXZ\x00",     // XZ compression
    b"Rar!\x1A\x07",     // RAR archives
    b"\x00asm",          // WebAssembly
];

/// Enhanced binary detection with magic bytes
pub fn is_binary_content(data: &[u8]) -> bool {
    if data.is_empty() {
        return false;
    }

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
    let printable = data
        .iter()
        .filter(|&&b| b == 9 || b == 10 || b == 13 || (32..127).contains(&b) || b >= 128)
        .count();

    printable < data.len() * 85 / 100
}

// =============================================================================
// SECRET DETECTION & REDACTION (wired into the merge path as of v7.2)
// =============================================================================

pub struct SecretRule {
    pub name: &'static str,
    pub regex: Regex,
    /// Generic key/value patterns must also pass an entropy gate so ordinary
    /// code (`password = input()`) is not redacted.
    pub entropy_gate: bool,
}

/// Merged ruleset: the v7.1 hardcoded patterns + the (previously never-loaded)
/// resources/secrets.json patterns (OpenAI, Anthropic, GitLab, SSN, credit card).
static SECRET_RULES: LazyLock<Vec<SecretRule>> = LazyLock::new(|| {
    let rule = |name: &'static str, re: &str, entropy_gate: bool| SecretRule {
        name,
        regex: Regex::new(re).expect("secret rule regex"),
        entropy_gate,
    };
    vec![
        // Cloud / VCS
        rule(
            "AWS Access Key ID",
            r"\b(AKIA|ASIA|AIDA|AROA)[A-Z0-9]{16}\b",
            false,
        ),
        rule(
            "AWS Secret Key",
            r#"(?i)aws[_\-.]?secret[_\-.]?access[_\-.]?key\s*[:=]\s*['"]?[A-Za-z0-9/+=]{40}['"]?"#,
            false,
        ),
        rule(
            "GitHub Token",
            r"\b(ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36,255}\b",
            false,
        ),
        rule(
            "GitHub Fine-grained PAT",
            r"\bgithub_pat_[A-Za-z0-9_]{22,255}\b",
            false,
        ),
        rule("GitLab Token", r"\bglpat-[A-Za-z0-9\-_]{20,}\b", false),
        // LLM providers (the very services this tool feeds)
        rule(
            "Anthropic API Key",
            r"\bsk-ant-[A-Za-z0-9\-_]{32,}\b",
            false,
        ),
        rule("OpenAI API Key", r"\bsk-[A-Za-z0-9\-_]{32,}\b", true),
        rule("Google API Key", r"\bAIza[0-9A-Za-z\-_]{35}\b", false),
        rule(
            "Google OAuth Client",
            r"\b[0-9]+-[a-z0-9]+\.apps\.googleusercontent\.com\b",
            false,
        ),
        // Payments / SaaS
        rule(
            "Stripe Key",
            r"\b[sr]k_(live|test)_[A-Za-z0-9]{24,}\b",
            false,
        ),
        rule("Slack Token", r"\bxox[baprs]-[0-9A-Za-z\-]{10,}\b", false),
        rule(
            "Slack Webhook",
            r"https://hooks\.slack\.com/services/T[A-Z0-9]+/B[A-Z0-9]+/[A-Za-z0-9]+",
            false,
        ),
        rule(
            "Azure Storage Key",
            r"DefaultEndpointsProtocol=https;AccountName=[^;]+;AccountKey=[A-Za-z0-9+/=]{88};",
            false,
        ),
        rule("npm Token", r"\bnpm_[A-Za-z0-9]{36}\b", false),
        rule("Twilio API Key", r"\bSK[a-f0-9]{32}\b", false),
        rule(
            "SendGrid API Key",
            r"\bSG\.[A-Za-z0-9_-]{22}\.[A-Za-z0-9_-]{43}\b",
            false,
        ),
        rule("Mailchimp API Key", r"\b[a-f0-9]{32}-us[0-9]{1,2}\b", false),
        rule(
            "Discord Token",
            r"\b[MN][A-Za-z\d]{23,}\.[\w-]{6}\.[\w-]{27}\b",
            false,
        ),
        rule(
            "Discord Webhook",
            r"https://discord(app)?\.com/api/webhooks/[0-9]+/[A-Za-z0-9_-]+",
            false,
        ),
        // Tokens / keys / URLs
        rule(
            "JWT Token",
            r"\beyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b",
            false,
        ),
        rule(
            "Private Key Block",
            r"-----BEGIN [A-Z ]{0,20}PRIVATE KEY( BLOCK)?-----",
            false,
        ),
        rule(
            "Database URL with password",
            r#"\b(postgres(ql)?|mysql|mongodb(\+srv)?|redis|amqps?)://[^:/\s]+:[^@\s]+@[^\s'"]+"#,
            false,
        ),
        rule(
            "Bearer Token",
            r"(?i)\bbearer\s+(?P<v>[A-Za-z0-9\-_.=]{20,})",
            true,
        ),
        // Generic key/value — entropy-gated on the VALUE capture only
        rule(
            "Generic API Key",
            r#"(?i)\b(api[_\-.]?key|apikey)\s*[:=]\s*['"]?(?P<v>[A-Za-z0-9\-_]{20,})['"]?"#,
            true,
        ),
        rule(
            "Generic Secret",
            r#"(?i)\b(secret|password|passwd|pwd)\s*[:=]\s*['"]?(?P<v>[^\s'"]{8,})['"]?"#,
            true,
        ),
        // PII (from the former secrets.json)
        rule("Possible SSN", r"\b\d{3}-\d{2}-\d{4}\b", false),
        rule(
            "Credit Card Number",
            r"\b(?:4[0-9]{12}(?:[0-9]{3})?|5[1-5][0-9]{14}|3[47][0-9]{13})\b",
            false,
        ),
    ]
});

/// Placeholder values that should never count as secrets.
const SECRET_STOPWORDS: &[&str] = &[
    "example",
    "sample",
    "changeme",
    "change_me",
    "placeholder",
    "your_",
    "your-",
    "yourkey",
    "xxxx",
    "dummy",
    "insert_",
    "<",
    "${",
    "{{",
    "todo",
];

#[derive(Debug, Clone)]
pub struct RedactionEvent {
    pub rule: &'static str,
    pub count: usize,
}

/// Minimum Shannon entropy (bits/char) for entropy-gated rules.
const ENTROPY_THRESHOLD: f64 = 3.3;

/// Redact secrets in `content`. Returns the redacted text plus one event per
/// rule that fired (with match counts). Matches containing placeholder
/// stopwords are left alone; entropy-gated rules skip low-entropy matches.
pub fn redact_secrets(content: &str) -> (String, Vec<RedactionEvent>) {
    let mut text = content.to_string();
    let mut events = Vec::new();

    for rule in SECRET_RULES.iter() {
        if !rule.regex.is_match(&text) {
            continue;
        }
        let mut count = 0usize;
        let mut out = String::with_capacity(text.len());
        let mut last = 0usize;
        for caps in rule.regex.captures_iter(&text) {
            let m = caps.get(0).expect("capture 0 always present");
            let s = m.as_str();
            let lower = s.to_ascii_lowercase();
            // entropy is judged on the value capture when the rule has one,
            // so key names ("password = ") don't inflate it
            let gate_target = caps.name("v").map(|v| v.as_str()).unwrap_or(s);
            let benign = SECRET_STOPWORDS.iter().any(|w| lower.contains(w))
                || (rule.entropy_gate && calculate_entropy(gate_target) < ENTROPY_THRESHOLD);
            out.push_str(&text[last..m.start()]);
            if benign {
                out.push_str(s);
            } else {
                out.push_str("[REDACTED]");
                count += 1;
            }
            last = m.end();
        }
        out.push_str(&text[last..]);
        text = out;
        if count > 0 {
            events.push(RedactionEvent {
                rule: rule.name,
                count,
            });
        }
    }

    (text, events)
}

/// Shannon entropy (bits per character)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_blocks_real_credential_files() {
        for name in [
            ".env",
            ".env.local",
            ".env.production",
            "id_rsa",
            "id_ed25519.pub",
            "server.pem",
            "cert.pfx",
            "signing.key",
            "credentials.json",
            ".npmrc",
        ] {
            assert!(
                sensitive_reason(Path::new(name)).is_some(),
                "{} should be sensitive",
                name
            );
        }
    }

    #[test]
    fn sensitive_allows_ordinary_source_files() {
        for name in [
            "password_reset.py",
            "reset_password.tsx",
            "useApiKey.ts",
            "credentials_form.tsx",
            "config.environments.ts",
            "deployment.envoy.yaml",
            "app.key.ts",
            ".env.example",
        ] {
            assert!(
                sensitive_reason(Path::new(name)).is_none(),
                "{} should NOT be sensitive",
                name
            );
        }
    }

    #[test]
    fn sensitive_blocks_credential_data_files() {
        // The user's `<NAME>_CREDENTIALS_<UTC>.md` convention + common dumps
        for name in [
            "SSM_CREDENTIALS_2026-06-29T1200Z.md",
            "passwords.csv",
            "vault.txt",
            "prod.secrets.yaml",
            "api_keys.json",
            "MyPasswords.txt",
        ] {
            assert!(
                sensitive_reason(Path::new(name)).is_some(),
                "{} should be sensitive (credential data file)",
                name
            );
        }
        // …but a source file that merely mentions the word is fine
        for name in ["credentialize.rs", "password_strength.ts", "secretsanta.py"] {
            assert!(
                sensitive_reason(Path::new(name)).is_none(),
                "{} should NOT be sensitive",
                name
            );
        }
    }

    #[test]
    fn redacts_known_token_shapes() {
        let input = concat!(
            "github: ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n",
            "aws: AKIAIOSFODNN7EXAMPL0\n",
            "anthropic: sk-ant-abc123def456ghi789jkl012mno345pqr678\n",
            "-----BEGIN RSA PRIVATE KEY-----\n",
        );
        let (out, events) = redact_secrets(input);
        assert!(
            !out.contains("ghp_AAAA"),
            "github token not redacted: {}",
            out
        );
        assert!(!out.contains("AKIAIOSFODNN7EXAMPL0"));
        assert!(!out.contains("sk-ant-abc123"));
        assert!(!out.contains("BEGIN RSA PRIVATE KEY"));
        assert!(out.matches("[REDACTED]").count() >= 4);
        assert!(!events.is_empty());
    }

    #[test]
    fn leaves_ordinary_code_alone() {
        let input = "let password = input();\nconst apiKey = getKeyFromVault;\npwd = user_pwd\n";
        let (out, events) = redact_secrets(input);
        assert_eq!(out, input, "ordinary code must survive redaction untouched");
        assert!(events.is_empty());
    }

    #[test]
    fn stopwords_suppress_placeholders() {
        let input = "password = \"changeme-please\"\napi_key = 'your_key_here_1234567890'\n";
        let (out, _) = redact_secrets(input);
        assert_eq!(out, input);
    }

    #[test]
    fn blocked_roots_only_at_top_level() {
        assert!(validate_and_canonicalize("C:\\Windows").is_err());
        // a *deep* folder named windows is not a system path — validated via components logic
        let tmp = std::env::temp_dir().join("tm_sec_test").join("windows");
        std::fs::create_dir_all(&tmp).unwrap();
        assert!(
            validate_and_canonicalize(&tmp.to_string_lossy()).is_ok(),
            "deep 'windows' folder should be allowed"
        );
        let _ = std::fs::remove_dir_all(tmp.parent().unwrap());
    }

    #[test]
    fn entropy_sane() {
        assert!(calculate_entropy("aaaaaaaaaa") < 1.0);
        assert!(calculate_entropy("k9X!qPz$7Lm@2Wv#") > 3.5);
    }
}
