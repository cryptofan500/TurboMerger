//! Remote repo packing (T2-8): accept a GitHub-style URL or `owner/repo`
//! shorthand, shallow-clone it into a self-cleaning temp dir, and hand the
//! checkout to the normal scan/merge pipeline.
//!
//! Credentials: an optional PAT is held in memory only, injected into the
//! clone URL, never logged; git stderr is scrubbed before it can surface.

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

    if let Some(rest) = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://")) {
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

    if s.starts_with("git@") && s.contains(':') {
        return Some((s.to_string(), repo_name(s.split(':').nth(1)?)?));
    }

    // owner/repo shorthand: exactly one slash, no path-ish characters, and
    // not something that exists locally (the caller double-checks too).
    let parts: Vec<&str> = s.split('/').collect();
    if parts.len() == 2
        && !s.starts_with('.')
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
        && !Path::new(s).exists()
    {
        let url = format!("https://github.com/{}/{}.git", parts[0], parts[1]);
        return Some((url, parts[1].trim_end_matches(".git").to_string()));
    }
    None
}

/// Inject a PAT into an https clone URL (GitHub's x-access-token convention).
fn with_pat(url: &str, pat: &str) -> String {
    match url.strip_prefix("https://") {
        Some(rest) if !pat.is_empty() => format!("https://x-access-token:{}@{}", pat, rest),
        _ => url.to_string(),
    }
}

/// Remove the PAT anywhere it could echo back (git prints the URL on failure).
fn scrub(text: &str, pat: Option<&str>) -> String {
    match pat {
        Some(p) if !p.is_empty() => text.replace(p, "***"),
        _ => text.to_string(),
    }
}

/// Shallow-clone `url` into a fresh temp dir. `pat` (optional) is used for
/// https auth and scrubbed from any error output.
pub fn clone_shallow(
    url: &str,
    repo_name: &str,
    pat: Option<&str>,
) -> Result<RemoteCheckout, String> {
    let tmp = tempfile::Builder::new()
        .prefix("turbomerger_remote_")
        .tempdir()
        .map_err(|e| format!("temp dir: {}", e))?;
    let target = tmp.path().join(repo_name);

    let auth_url = match pat {
        Some(p) => with_pat(url, p),
        None => url.to_string(),
    };

    let mut cmd = std::process::Command::new("git");
    cmd.arg("clone")
        .arg("--depth")
        .arg("1")
        .arg("--single-branch")
        .arg("--no-tags")
        .arg(&auth_url)
        .arg(&target)
        // Fail fast instead of prompting for credentials in a GUI process.
        .env("GIT_TERMINAL_PROMPT", "0");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("git not runnable: {}", e))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
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
    fn pat_injected_and_scrubbed() {
        assert_eq!(
            with_pat("https://github.com/o/r.git", "TOKEN123"),
            "https://x-access-token:TOKEN123@github.com/o/r.git"
        );
        // ssh URLs pass through untouched
        assert_eq!(
            with_pat("git@github.com:o/r.git", "TOKEN123"),
            "git@github.com:o/r.git"
        );
        assert_eq!(
            scrub("fatal: repo 'https://x-access-token:TOKEN123@x/y'", Some("TOKEN123")),
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
            origin.to_string_lossy().replace('\\', "/").trim_start_matches('/')
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
        assert!(
            !checkout_path.exists(),
            "checkout must self-clean on drop"
        );
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
