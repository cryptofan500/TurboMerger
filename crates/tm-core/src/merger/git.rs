//! Git context (T2-5): the working-tree diff and recent log as synthetic
//! blocks at the end of the merge.

use std::collections::HashSet;
use std::path::Path;

use super::{Block, MergeConfig};
use crate::scanner::{SkipEntry, SkipKind};
use crate::tokens;

/// Cap for the diff section so a giant rebase can't dwarf the codebase.
const GIT_DIFF_MAX_BYTES: usize = 512 * 1024;

/// Run git read-only against `root`. Repository config must not be able to
/// run programs while we read it (a cloned repo's `.git/config` is
/// attacker-controlled): the fsmonitor hook is off, the pager is `cat`,
/// optional index locks/refreshes are skipped, and diff callers add
/// `--no-ext-diff --no-textconv` (N-24).
fn run_git(root: &Path, args: &[&str]) -> std::result::Result<String, String> {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C")
        .arg(root)
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.pager=cat",
            "-c",
            "core.quotePath=false",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null());
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
        return Err(err.lines().next().unwrap_or("git failed").to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Unquote a git path (C-style quoting for names with specials).
fn git_unquote(p: &str) -> String {
    let Some(inner) = p.strip_prefix('"').and_then(|q| q.strip_suffix('"')) else {
        return p.to_string();
    };
    let mut bytes = Vec::with_capacity(inner.len());
    let mut it = inner.bytes().peekable();
    while let Some(b) = it.next() {
        if b != b'\\' {
            bytes.push(b);
            continue;
        }
        match it.next() {
            Some(b'n') => bytes.push(b'\n'),
            Some(b't') => bytes.push(b'\t'),
            Some(b'r') => bytes.push(b'\r'),
            Some(d @ b'0'..=b'7') => {
                let mut v = (d - b'0') as u32;
                for _ in 0..2 {
                    if let Some(&n @ b'0'..=b'7') = it.peek() {
                        v = v * 8 + (n - b'0') as u32;
                        it.next();
                    }
                }
                bytes.push(v as u8);
            }
            Some(other) => bytes.push(other),
            None => {}
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The path one `diff --git` section is about: its new name, or its old
/// name for a deletion. `(path, deleted)`.
fn diff_section_path(section: &str) -> Option<(String, bool)> {
    let (mut old, mut new): (Option<String>, Option<String>) = (None, None);
    for line in section.lines() {
        if let Some(p) = line.strip_prefix("--- ") {
            old = Some(p.to_string());
        } else if let Some(p) = line.strip_prefix("+++ ") {
            new = Some(p.to_string());
        } else if let Some(p) = line.strip_prefix("rename to ") {
            new = Some(format!("b/{}", p));
        } else if let Some(rest) = line.strip_prefix("Binary files ") {
            if let Some((a, b)) = rest.trim_end_matches(" differ").split_once(" and ") {
                old = Some(a.to_string());
                new = Some(b.to_string());
            }
        }
        if line.starts_with("@@") {
            break;
        }
    }
    let strip = |p: &str| {
        let p = git_unquote(p.split('\t').next().unwrap_or(p));
        p.strip_prefix("a/")
            .or_else(|| p.strip_prefix("b/"))
            .map(str::to_string)
            .unwrap_or(p)
    };
    match (old.as_deref(), new.as_deref()) {
        (_, Some(n)) if n != "/dev/null" => Some((strip(n), false)),
        (Some(o), _) if o != "/dev/null" => Some((strip(o), true)),
        _ => {
            // Mode-only / empty-file sections: `diff --git a/p b/p`.
            let header = section.lines().next()?.strip_prefix("diff --git ")?;
            let half = header.len() / 2;
            let (a, b) = header.split_at(half);
            (a.trim_end().strip_prefix("a/")? == b.trim_start().strip_prefix("b/")?)
                .then(|| (b.trim_start()[2..].to_string(), false))
        }
    }
}

/// Keep only the diff sections of files that are in this merge (N-24:
/// `git diff HEAD` covered the whole repository, including files the merge
/// deliberately excluded). Deleted files are kept unless their name marks
/// them sensitive. Dropped sections are reported, not silently lost.
fn scope_diff(diff: &str, included: &HashSet<String>, skips: &mut Vec<SkipEntry>) -> String {
    let mut out = String::with_capacity(diff.len());
    let mut sections: Vec<String> = Vec::new();
    for line in diff.split_inclusive('\n') {
        if line.starts_with("diff --git ") || sections.is_empty() {
            sections.push(String::new());
        }
        sections.last_mut().expect("section").push_str(line);
    }
    for section in sections {
        let parsed = diff_section_path(&section);
        let keep = match &parsed {
            Some((p, false)) => included.contains(p),
            Some((p, true)) => crate::security::sensitive_reason(Path::new(p)).is_none(),
            None => false,
        };
        if keep {
            out.push_str(&section);
        } else {
            let path = parsed
                .map(|(p, _)| p)
                .unwrap_or_else(|| "(unparsed section)".into());
            skips.push(SkipEntry::new(
                format!("GIT DIFF: {}", path),
                "diff section omitted — the file is not in this merge",
                SkipKind::Excluded,
            ));
        }
    }
    out
}

/// Build the synthetic git-context blocks. Failures (not a repo, no git) are
/// reported as skip entries, never errors — git context is best-effort.
pub(super) fn git_context_blocks(
    root: &Path,
    cfg: &MergeConfig,
    included: &HashSet<String>,
    skips: &mut Vec<SkipEntry>,
) -> Vec<(Block, String)> {
    let mut blocks = Vec::new();
    let mut push = |relative: &str, lang: &str, mut content: String, cfg: &MergeConfig| {
        let redactions = if cfg.redact {
            let (clean, events) = crate::security::redact_secrets(&content);
            content = clean;
            events.into_iter().map(|ev| (ev.rule, ev.count)).collect()
        } else {
            Vec::new()
        };
        let tokens = tokens::count(&content);
        blocks.push((
            Block {
                relative: relative.to_string(),
                span: Default::default(),
                lang: lang.to_string(),
                tokens,
                utf8_note: None,
                redactions,
                compressed: false,
                synthetic: true,
            },
            content,
        ));
    };

    if cfg.git_diff {
        match run_git(
            root,
            &[
                "diff",
                "HEAD",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--relative",
                "--",
                ".",
            ],
        )
        .map(|d| scope_diff(&d, included, skips))
        {
            Ok(diff) if diff.trim().is_empty() => skips.push(SkipEntry::new(
                "GIT DIFF",
                "no changes to the merged files — no diff section",
                SkipKind::Excluded,
            )),
            Ok(mut diff) => {
                if diff.len() > GIT_DIFF_MAX_BYTES {
                    let mut cut = GIT_DIFF_MAX_BYTES;
                    while !diff.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    diff.truncate(cut);
                    diff.push_str("\n[... diff truncated at 512 KB ...]\n");
                }
                push("GIT DIFF (working tree vs HEAD)", "diff", diff, cfg);
            }
            Err(e) => skips.push(SkipEntry::new(
                "GIT DIFF",
                format!("git diff unavailable: {}", e),
                SkipKind::NotCaptured,
            )),
        }
    }
    if cfg.git_log > 0 {
        let n = cfg.git_log.to_string();
        match run_git(
            root,
            &[
                "log",
                "-n",
                &n,
                "--pretty=format:%h %ad %an — %s",
                "--date=short",
                "--no-color",
                "--",
                ".",
            ],
        ) {
            Ok(log) if log.trim().is_empty() => {
                skips.push(SkipEntry::new("GIT LOG", "no commits", SkipKind::Excluded))
            }
            Ok(log) => {
                let title = format!("GIT LOG (last {} commits)", cfg.git_log);
                push(&title, "", log, cfg);
            }
            Err(e) => skips.push(SkipEntry::new(
                "GIT LOG",
                format!("git log unavailable: {}", e),
                SkipKind::NotCaptured,
            )),
        }
    }
    blocks
}
