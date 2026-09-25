//! Progress, deadline, partial output and stall reports of the real binary
//! (plan §10, Phase 2.9). File processing is slowed with the debug-only
//! `TM_TEST_SLOW_FILE_MS` hook so the timing is not a race.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn tm(args: &[&str], cwd: &Path, slow_ms: u64) -> Output {
    Command::new(env!("CARGO_BIN_EXE_turbomerger"))
        .args(args)
        .current_dir(cwd)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env("TURBOMERGER_STATE_DIR", cwd.join("state"))
        .env("TURBOMERGER_CACHE_DIR", cwd.join("cache"))
        .env("TM_TEST_SLOW_FILE_MS", slow_ms.to_string())
        .output()
        .expect("binary runs")
}

fn corpus(root: &Path, n: usize) {
    fs::create_dir_all(root).unwrap();
    for i in 0..n {
        fs::write(
            root.join(format!("f{i:03}.rs")),
            format!("fn f{i}() {{}}\n"),
        )
        .unwrap();
    }
}

fn summary_field(stdout: &str, key: &str) -> usize {
    stdout
        .split_whitespace()
        .find_map(|kv| kv.strip_prefix(&format!("{key}=")))
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("{key} in {stdout:?}"))
}

#[test]
fn deadline_partial_writes_what_is_done_and_exits_4() {
    let tmp = tempfile::tempdir().unwrap();
    corpus(&tmp.path().join("src"), 130);
    // 130 files = three chunks; each file takes 300 ms, so the first chunk
    // alone outlasts the 1 s deadline.
    let o = tm(
        &[
            "merge",
            "src",
            "out.md",
            "--deadline",
            "1s",
            "--on-deadline",
            "partial",
        ],
        tmp.path(),
        300,
    );
    let stdout = String::from_utf8_lossy(&o.stdout);
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert_eq!(o.status.code(), Some(4), "{stdout}\n{stderr}");
    let merged = summary_field(&stdout, "merged");
    let not_captured = summary_field(&stdout, "not_captured");
    assert!(merged > 0 && merged < 130, "{stdout}");
    assert_eq!(
        merged + not_captured,
        130,
        "every file accounted for: {stdout}"
    );
    assert!(stderr.contains("deadline of 1s reached"), "{stderr}");
    let out = fs::read_to_string(tmp.path().join("out.md")).unwrap();
    assert!(out.contains("not processed — the run was finished early (deadline)"));
}

#[test]
fn deadline_cancel_writes_nothing_and_exits_4() {
    let tmp = tempfile::tempdir().unwrap();
    corpus(&tmp.path().join("src"), 70);
    let o = tm(
        &["merge", "src", "out.md", "--deadline", "1s"],
        tmp.path(),
        300,
    );
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert_eq!(o.status.code(), Some(4), "{stderr}");
    assert!(
        stderr.contains("cancelled (deadline) — nothing was written"),
        "{stderr}"
    );
    let left: Vec<_> = fs::read_dir(tmp.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "src" && n != "state")
        .collect();
    assert!(left.is_empty(), "no output, no temp files: {left:?}");
}

#[test]
fn a_hang_is_reported_as_a_stall_and_json_progress_flows() {
    let tmp = tempfile::tempdir().unwrap();
    corpus(&tmp.path().join("src"), 2);
    let o = tm(
        &[
            "merge",
            "src",
            "out.md",
            "--progress",
            "json",
            "--stall-after",
            "1s",
            "--eta-after",
            "0s",
        ],
        tmp.path(),
        2500,
    );
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert_eq!(o.status.code(), Some(0), "{stderr}");
    let stall = stderr
        .lines()
        .find(|l| l.contains("\"event\":\"stall\""))
        .unwrap_or_else(|| panic!("a stall event: {stderr}"));
    assert!(
        stall.contains("\"current\":\"f000.rs\""),
        "names the file: {stall}"
    );
    assert!(
        stderr
            .lines()
            .filter(|l| l.contains("\"event\":\"progress\""))
            .count()
            >= 1,
        "{stderr}"
    );

    // --on-stall fail turns the same hang into a cancel.
    let o = tm(
        &[
            "merge",
            "src",
            "out2.md",
            "--stall-after",
            "1s",
            "--on-stall",
            "fail",
        ],
        tmp.path(),
        2500,
    );
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert_eq!(o.status.code(), Some(4), "{stderr}");
    assert!(stderr.contains("stalled on f000.rs"), "{stderr}");
    assert!(!tmp.path().join("out2.md").exists());
}
