//! Build stamp for `turbomerger --version` (E2, 2026-11-07):
//! `7.7.0+<commit> (<commit date>)`, with `.dirty` when the tracked files
//! differ from that commit. The Dev build printed a bare 7.7.0, the same as
//! the retired 2026-08 package, so the two could not be told apart.

use std::process::Command;

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(args)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!s.is_empty()).then_some(s)
    };
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let stamp = match git(&["rev-parse", "--short=7", "HEAD"]) {
        Some(sha) => {
            let date = git(&["show", "-s", "--format=%cs", "HEAD"]).unwrap_or_default();
            let dirty = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["diff", "--quiet", "HEAD", "--"])
                .status()
                .is_ok_and(|s| !s.success());
            let dirty = if dirty { ".dirty" } else { "" };
            format!("{version}+{sha}{dirty} ({date})")
        }
        // No git (a source archive): the plain version.
        None => version,
    };
    println!("cargo:rustc-env=TM_VERSION_STAMP={stamp}");

    // Re-run when HEAD, the branch, the index or the sources change, so the
    // stamp never lags behind what was compiled.
    if let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
        println!("cargo:rerun-if-changed={git_dir}/index");
        if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
            println!("cargo:rerun-if-changed={git_dir}/{branch}");
        }
    }
    for sub in ["../../crates", "../../apps", "../../src-tauri/src"] {
        let p = std::path::Path::new(&dir).join(sub);
        if p.exists() {
            println!("cargo:rerun-if-changed={}", p.display());
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
}
