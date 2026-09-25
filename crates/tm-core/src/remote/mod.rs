//! Remote repo packing (T2-8): accept a GitHub-style URL or `owner/repo`
//! shorthand, shallow-clone it into a self-cleaning temp dir, and hand the
//! checkout to the normal scan/merge pipeline.
//!
//! Credentials: an optional PAT is held in memory only and handed to git as
//! an HTTP `Authorization` header through `GIT_CONFIG_*` environment
//! variables (the `actions/checkout` mechanism) — never in argv, where any
//! local user can read it, and never in the clone's `.git/config`, where it
//! would survive a crash (N-22). git stderr is scrubbed before it surfaces.
//!
//! Only explicit references clone (N-23): `https://…`, `git@host:…`, or
//! `gh:owner/repo`. A bare `owner/repo` is a local path to the CLI and MCP —
//! a typo like `merge docs/api` in the wrong folder no longer packs
//! github.com/docs/api. (The GUI's remote box still accepts the shorthand:
//! typing into it is the explicit intent.)

use std::path::{Path, PathBuf};

/// A shallow checkout that deletes itself on drop.
#[derive(Debug)]
pub struct RemoteCheckout {
    /// Repo root (a `<repo-name>/` dir inside the temp dir, so output naming
    /// and the tree header show the repo name, not a temp hash).
    pub path: PathBuf,
    _tmp: tempfile::TempDir,
}

/// Normalize user input to (clone_url, repo_name). Accepts:
/// - `https://github.com/owner/repo[.git][/]` (any https host)
/// - `git@host:owner/repo[.git]`
/// - `owner/repo` shorthand → github.com
///
/// Returns None for anything that doesn't look like a remote repo reference
/// (notably local paths).
pub fn parse_remote(input: &str) -> Option<(String, String)> {
    let s = input.trim();
    if s.is_empty() || s.contains(char::is_whitespace) {
        return None;
    }

    let repo_name = |path: &str| -> Option<String> {
        let name = path.trim_end_matches('/').rsplit('/').next()?;
        let name = name.strip_suffix(".git").unwrap_or(name);
        if name.is_empty() {
            None
        } else {
            Some(name.to_string())
        }
    };

    if let Some(rest) = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
    {
        // host/owner/repo at minimum
        let (host, path) = rest.trim_end_matches('/').split_once('/')?;
        if host.is_empty() || !host.contains('.') || path.split('/').count() < 2 {
            return None;
        }
        // Drop /tree/<branch>/... and /blob/... suffixes from pasted links.
        let path = match path.find("/tree/").or_else(|| path.find("/blob/")) {
            Some(cut) => &path[..cut],
            None => path,
        };
        let url = format!("https://{}/{}", host, path.trim_end_matches('/'));
        let url = if url.ends_with(".git") {
            url
        } else {
            format!("{}.git", url)
        };
        return Some((url.clone(), repo_name(&url)?));
    }

    if let Some(rest) = s.strip_prefix("gh:") {
        return parse_shorthand(rest);
    }

    if let Some(rest) = s.strip_prefix("git@") {
        // Host must look like a host: a leading '-' would reach ssh as an
        // option (`git@-oProxyCommand=…:x/y`).
        let (host, _) = rest.split_once(':')?;
        if host.is_empty()
            || host.starts_with('-')
            || !host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
        {
            return None;
        }
        return Some((s.to_string(), repo_name(s.split(':').nth(1)?)?));
    }

    // owner/repo shorthand: exactly one slash, no path-ish characters, and
    // not something that exists locally (the caller double-checks too).
    if Path::new(s).exists() {
        return None;
    }
    parse_shorthand(s)
}

/// `owner/repo` → github.com clone URL.
fn parse_shorthand(s: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = s.split('/').collect();
    if parts.len() == 2
        && !s.starts_with('.')
        && !s.starts_with('-')
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
    {
        let url = format!("https://github.com/{}/{}.git", parts[0], parts[1]);
        return Some((url, parts[1].trim_end_matches(".git").to_string()));
    }
    None
}

/// Remote references the CLI and MCP accept: full URLs and `gh:owner/repo`
/// only — never a bare `owner/repo`, which is a local path there (N-23).
pub fn parse_remote_explicit(input: &str) -> Option<(String, String)> {
    let s = input.trim();
    let explicit = s.starts_with("https://")
        || s.starts_with("http://")
        || s.starts_with("git@")
        || s.starts_with("gh:");
    if explicit {
        parse_remote(s)
    } else {
        None
    }
}

/// Host of an https clone URL (`https://host/owner/repo.git` → `host`).
fn https_host(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://")?;
    let host = rest.split('/').next()?;
    (!host.is_empty() && !host.contains('@')).then_some(host)
}

/// Standard base64 (for the basic-auth header; no dependency needed).
fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// `GIT_CONFIG_*` environment for an https PAT: an `Authorization` header
/// scoped to the clone's host. `None` for SSH URLs or an empty PAT.
fn pat_env(url: &str, pat: &str) -> Option<[(String, String); 3]> {
    if pat.is_empty() {
        return None;
    }
    let host = https_host(url)?;
    Some([
        ("GIT_CONFIG_COUNT".into(), "1".into()),
        (
            "GIT_CONFIG_KEY_0".into(),
            format!("http.https://{}/.extraheader", host),
        ),
        (
            "GIT_CONFIG_VALUE_0".into(),
            format!(
                "AUTHORIZATION: basic {}",
                base64(format!("x-access-token:{}", pat).as_bytes())
            ),
        ),
    ])
}

/// Remove the PAT anywhere it could echo back (git prints the URL on failure).
fn scrub(text: &str, pat: Option<&str>) -> String {
    match pat {
        Some(p) if !p.is_empty() => text.replace(p, "***"),
        _ => text.to_string(),
    }
}

/// Default clone timeout; `TURBOMERGER_CLONE_TIMEOUT` (seconds) overrides it.
const CLONE_TIMEOUT_SECS: u64 = 300;

/// Shallow-clone `url` into a fresh temp dir. `pat` (optional) is used for
/// https auth via the environment and scrubbed from any error output. Git
/// LFS smudging is skipped (it can pull gigabytes) and the clone is killed
/// after a timeout.
pub fn clone_shallow(
    url: &str,
    repo_name: &str,
    pat: Option<&str>,
) -> Result<RemoteCheckout, String> {
    use std::io::Read;

    let tmp = tempfile::Builder::new()
        .prefix("turbomerger_remote_")
        .tempdir()
        .map_err(|e| format!("temp dir: {}", e))?;
    let target = tmp.path().join(repo_name);

    let mut cmd = std::process::Command::new("git");
    cmd.arg("clone")
        .arg("--depth")
        .arg("1")
        .arg("--single-branch")
        .arg("--no-tags")
        .arg("--")
        .arg(url)
        .arg(&target)
        // Fail fast instead of prompting for credentials in a GUI process.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_LFS_SKIP_SMUDGE", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    if let Some(env) = pat.and_then(|p| pat_env(url, p)) {
        for (k, v) in env {
            cmd.env(k, v);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("git not runnable: {}", e))?;
    let mut stderr = child.stderr.take().expect("piped stderr");
    let reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf);
        buf
    });
    let timeout = std::env::var("TURBOMERGER_CLONE_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(CLONE_TIMEOUT_SECS);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("clone timed out after {} s", timeout));
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(e) => return Err(format!("git wait failed: {}", e)),
        }
    };
    let err = reader.join().unwrap_or_default();
    if !status.success() {
        let first = err
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("git clone failed");
        return Err(format!("clone failed: {}", scrub(first, pat)));
    }
    if !target.is_dir() {
        return Err("clone produced no checkout".to_string());
    }
    Ok(RemoteCheckout {
        path: target,
        _tmp: tmp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_urls_shorthand_and_ssh() {
        assert_eq!(
            parse_remote("https://github.com/cryptofan500/TurboMerger"),
            Some((
                "https://github.com/cryptofan500/TurboMerger.git".to_string(),
                "TurboMerger".to_string()
            ))
        );
        assert_eq!(
            parse_remote("https://github.com/owner/repo.git/"),
            Some((
                "https://github.com/owner/repo.git".to_string(),
                "repo".to_string()
            ))
        );
        assert_eq!(
            parse_remote("https://github.com/owner/repo/tree/main/src"),
            Some((
                "https://github.com/owner/repo.git".to_string(),
                "repo".to_string()
            ))
        );
        assert_eq!(
            parse_remote("https://gitlab.com/group/project"),
            Some((
                "https://gitlab.com/group/project.git".to_string(),
                "project".to_string()
            ))
        );
        assert_eq!(
            parse_remote("git@github.com:owner/repo.git"),
            Some((
                "git@github.com:owner/repo.git".to_string(),
                "repo".to_string()
            ))
        );
        assert_eq!(
            parse_remote("rust-lang/cargo"),
            Some((
                "https://github.com/rust-lang/cargo.git".to_string(),
                "cargo".to_string()
            ))
        );
    }

    #[test]
    fn explicit_references_only_for_cli_and_mcp() {
        assert_eq!(
            parse_remote_explicit("gh:rust-lang/cargo"),
            Some((
                "https://github.com/rust-lang/cargo.git".to_string(),
                "cargo".to_string()
            ))
        );
        assert!(parse_remote_explicit("https://github.com/o/r").is_some());
        assert!(parse_remote_explicit("git@github.com:o/r.git").is_some());
        // A bare owner/repo is a (missing) local path, never a clone.
        assert_eq!(parse_remote_explicit("docs/api"), None);
        assert_eq!(parse_remote_explicit("rust-lang/cargo"), None);
        // ssh option injection through the host part is refused.
        assert_eq!(parse_remote("git@-oProxyCommand=touch pwned:x/y"), None);
        assert_eq!(parse_remote("git@-oProxyCommand=x:x/y"), None);
    }

    #[test]
    fn parse_rejects_local_paths_and_noise() {
        assert_eq!(parse_remote("C:/Users/admin/project"), None);
        assert_eq!(parse_remote("src"), None); // no slash
        assert_eq!(parse_remote("./src/module"), None);
        assert_eq!(parse_remote("https://github.com/only-owner"), None);
        assert_eq!(parse_remote("owner/repo/extra"), None);
        assert_eq!(parse_remote("owner/re po"), None);
        assert_eq!(parse_remote(""), None);
        // an owner/repo-shaped path that EXISTS locally is a local path
        let tmp = tempfile::tempdir().unwrap();
        let local = tmp.path().join("owner2").join("repo2");
        std::fs::create_dir_all(&local).unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        assert_eq!(parse_remote("owner2/repo2"), None);
        std::env::set_current_dir(prev).unwrap();
    }

    #[test]
    fn pat_goes_to_a_host_scoped_header_never_the_url() {
        let env = pat_env("https://github.com/o/r.git", "TOKEN123").unwrap();
        assert_eq!(env[1].1, "http.https://github.com/.extraheader");
        // base64("x-access-token:TOKEN123")
        assert_eq!(
            env[2].1,
            "AUTHORIZATION: basic eC1hY2Nlc3MtdG9rZW46VE9LRU4xMjM="
        );
        assert!(env.iter().all(|(_, v)| !v.contains("TOKEN123")));
        // ssh URLs and empty tokens get no header
        assert!(pat_env("git@github.com:o/r.git", "TOKEN123").is_none());
        assert!(pat_env("https://github.com/o/r.git", "").is_none());
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(
            scrub(
                "fatal: repo 'https://x-access-token:TOKEN123@x/y'",
                Some("TOKEN123")
            ),
            "fatal: repo 'https://x-access-token:***@x/y'"
        );
    }

    #[test]
    fn clone_shallow_works_from_local_file_url_and_cleans_up() {
        let run_git = |root: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .expect("git runs");
            assert!(
                out.status.success(),
                "git {:?}: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin_repo");
        std::fs::create_dir_all(&origin).unwrap();
        run_git(&origin, &["init", "-q"]);
        run_git(&origin, &["config", "user.email", "t@example.com"]);
        run_git(&origin, &["config", "user.name", "tester"]);
        run_git(&origin, &["config", "commit.gpgsign", "false"]);
        std::fs::write(origin.join("hello.rs"), "fn hello() {}\n").unwrap();
        run_git(&origin, &["add", "."]);
        run_git(&origin, &["commit", "-q", "-m", "seed"]);

        let url = format!(
            "file:///{}",
            origin
                .to_string_lossy()
                .replace('\\', "/")
                .trim_start_matches('/')
        );
        let checkout_path;
        {
            let co = clone_shallow(&url, "origin_repo", None).expect("clone works");
            checkout_path = co.path.clone();
            assert!(checkout_path.join("hello.rs").is_file());
            assert!(checkout_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("origin_repo"));
        }
        assert!(!checkout_path.exists(), "checkout must self-clean on drop");
    }

    #[test]
    fn clone_failure_error_is_scrubbed() {
        // localhost:1 refuses instantly — no real network needed.
        let err = clone_shallow(
            "https://localhost:1/nobody/nothing.git",
            "nothing",
            Some("SUPERSECRETPAT"),
        )
        .expect_err("must fail");
        assert!(
            !err.contains("SUPERSECRETPAT"),
            "PAT leaked into error: {}",
            err
        );
    }
}
