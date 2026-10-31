//! S8 1.6 (DeepSeek B3.8): a parser that panics on one malformed file must
//! cost that file only. Release builds unwind (Cargo.toml) so the per-file
//! guard can catch it; the debug-only `TM_TEST_PANIC_FILE` hook panics
//! inside the per-file work for the named file.

use std::fs;
use std::process::Command;

#[test]
fn a_parser_panic_loses_one_file_not_the_run() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("good.txt"), "kept\n").unwrap();
    fs::write(src.join("boom.txt"), "crashes the parser\n").unwrap();
    let out = tmp.path().join("out.md");
    let run = Command::new(env!("CARGO_BIN_EXE_turbomerger"))
        .args(["merge", src.to_str().unwrap(), out.to_str().unwrap()])
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env("TURBOMERGER_STATE_DIR", tmp.path().join("state"))
        .env("TURBOMERGER_CACHE_DIR", tmp.path().join("cache"))
        .env("TM_TEST_PANIC_FILE", "boom.txt")
        .output()
        .expect("binary runs");
    assert_eq!(
        run.status.code(),
        Some(3),
        "exit 3 = completed with content not captured; stderr: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let text = fs::read_to_string(&out).unwrap();
    assert!(text.contains("## good.txt"), "{text}");
    assert!(
        text.contains("`boom.txt` — the parser crashed on this file"),
        "{text}"
    );
}
