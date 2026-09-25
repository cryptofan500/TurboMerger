//! Apply-back path policy (N-52): which proposal paths may be written, and
//! how much confirmation each needs.
//!
//! An LLM reply is untrusted input. A reply that edits `src/main.rs` is what
//! the user asked for; a reply that also writes `.git/hooks/pre-commit`,
//! `.github/workflows/ci.yml` or a `.vscode/tasks.json` folder-open task
//! plants code that runs later without anyone asking. v7.7.0 applied all of
//! those as ordinary `modify`/`create` lines.
//!
//! Classes:
//! - **Forbidden** — inside a `.git` directory (hooks, `config` with
//!   `core.fsmonitor` / `core.hooksPath` / `core.sshCommand`, `info/`…). Git
//!   never versions these files, so no legitimate reply proposes them. Refused
//!   with no override.
//! - **Control** — files that make code run without the user asking: git
//!   attributes/modules, hook managers, CI pipelines, editor and coding-agent
//!   settings, environment loaders, package-manager/toolchain configs that
//!   execute code. Refused unless the run allows them (`--allow-control GLOB`,
//!   or a per-file confirmation in the GUI).
//! - **Manifest** — build and dependency manifests whose change runs code on
//!   the next build/install (`build.rs`, `package.json`, `setup.py`,
//!   `Makefile`, `requirements.txt`…). Written only after an explicit per-file
//!   confirmation (`--allow-manifest GLOB`, or the GUI confirmation).
//! - **Code** — everything else.
//!
//! Matching runs on a normalized copy of every component — ASCII-lowercased,
//! zero-width/bidi code points removed, trailing dots and spaces trimmed — so
//! the case-insensitive, HFS+ and NTFS aliases of `.git` (`.GIT`, `.g\u{200c}it`,
//! `.git.`, `git~1`) cannot slip past. The same check runs again on the path
//! the OS reports for the opened handle (`safe_fs::resolved_rel`), which
//! catches 8.3 short names and case variants on case-insensitive volumes.

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Serialize;

/// How a proposal path is treated by apply-back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PathClass {
    Code,
    Manifest,
    Control,
    Forbidden,
}

impl PathClass {
    pub fn as_str(self) -> &'static str {
        match self {
            PathClass::Code => "code",
            PathClass::Manifest => "manifest",
            PathClass::Control => "control",
            PathClass::Forbidden => "forbidden",
        }
    }
}

/// Classification plus the human reason shown in the preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classified {
    pub class: PathClass,
    pub reason: &'static str,
}

/// Directory names that are control wherever they appear (editors and agents
/// read nested copies too; cargo and direnv walk up from the working dir).
const CONTROL_DIRS_ANYWHERE: &[(&str, &str)] = &[
    (
        ".vscode",
        "editor settings/tasks can run commands when the folder opens",
    ),
    (".idea", "IDE settings can run commands"),
    (
        ".devcontainer",
        "dev-container config runs commands on open",
    ),
    (".zed", "editor tasks can run commands"),
    (
        ".claude",
        "agent settings can define hooks that run commands",
    ),
    (".cursor", "agent settings can run commands"),
    (".gemini", "agent settings can run commands"),
    (".codex", "agent settings can run commands"),
    (".cargo", "cargo config can set a rustc wrapper / runner"),
    (".yarn", "yarn plugins and releases are executed by yarn"),
    (".husky", "git hooks installed by husky"),
    (".githooks", "git hooks directory"),
];

/// File names that are control wherever they appear.
const CONTROL_FILES_ANYWHERE: &[(&str, &str)] = &[
    (".gitattributes", "git filters/diff drivers run commands"),
    (".gitmodules", "submodule URLs and update commands"),
    (".envrc", "direnv executes it when you cd into the folder"),
    (".tool-versions", "asdf/mise select and install toolchains"),
    (".mcp.json", "MCP servers are commands an agent launches"),
    (
        ".npmrc",
        "npm config can change registries and script shells",
    ),
    (".yarnrc", "yarn config can change registries"),
    (
        ".yarnrc.yml",
        "yarn config can load executable plugins (yarnPath)",
    ),
    (".pnpmfile.cjs", "pnpm executes it on install"),
    (
        "rust-toolchain",
        "selects the Rust toolchain (may point at a custom path)",
    ),
    (
        "rust-toolchain.toml",
        "selects the Rust toolchain (may point at a custom path)",
    ),
    (
        ".devcontainer.json",
        "dev-container config runs commands on open",
    ),
];

/// Root-relative files and directories read by CI systems and hook managers.
const CONTROL_ROOT_PREFIXES: &[(&str, &str)] = &[
    (".github/workflows/", "CI workflow — runs on push"),
    (".github/actions/", "CI action used by workflows"),
    (".gitlab-ci.yml", "CI pipeline — runs on push"),
    (".gitlab/ci/", "CI pipeline include"),
    (".circleci/", "CI pipeline — runs on push"),
    ("azure-pipelines.yml", "CI pipeline — runs on push"),
    (".azure-pipelines/", "CI pipeline — runs on push"),
    (".travis.yml", "CI pipeline — runs on push"),
    (".buildkite/", "CI pipeline — runs on push"),
    ("bitbucket-pipelines.yml", "CI pipeline — runs on push"),
    (".drone.yml", "CI pipeline — runs on push"),
    ("appveyor.yml", "CI pipeline — runs on push"),
    (".appveyor.yml", "CI pipeline — runs on push"),
    (".woodpecker/", "CI pipeline — runs on push"),
    (".woodpecker.yml", "CI pipeline — runs on push"),
    ("jenkinsfile", "CI pipeline — runs on push"),
    (
        ".pre-commit-config.yaml",
        "pre-commit hooks run on every commit",
    ),
    ("lefthook.yml", "git hooks run on every commit"),
    (".lefthook.yml", "git hooks run on every commit"),
    ("lefthook-local.yml", "git hooks run on every commit"),
];

/// Exact file names (lowercased) of build/dependency manifests.
const MANIFEST_FILES: &[&str] = &[
    "build.rs",
    "cargo.toml",
    "package.json",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "requirements.txt",
    "pipfile",
    "conftest.py",
    "noxfile.py",
    "tox.ini",
    "makefile",
    "gnumakefile",
    "cmakelists.txt",
    "meson.build",
    "build.zig",
    "gradlew",
    "gradlew.bat",
    "pom.xml",
    "build.xml",
    "directory.build.props",
    "directory.build.targets",
    "dockerfile",
    "containerfile",
    "docker-compose.yml",
    "docker-compose.yaml",
    "compose.yml",
    "compose.yaml",
    "go.mod",
    "gemfile",
    "rakefile",
    "composer.json",
    "mix.exs",
    "rebar.config",
    "flake.nix",
    "justfile",
    "taskfile.yml",
];

/// Manifest suffixes (lowercased).
const MANIFEST_SUFFIXES: &[&str] = &[
    ".gradle",
    ".gradle.kts",
    ".csproj",
    ".vbproj",
    ".fsproj",
    ".gemspec",
];

/// Characters that some filesystems ignore or that render invisibly: zero-width
/// joiners/spaces, bidi controls, BOM. HFS+ drops several of them when
/// comparing names, so `.g\u{200c}it` == `.git` there (git's CVE-2014-9390).
fn is_ignorable(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{206A}'..='\u{206F}'
            | '\u{FEFF}'
    )
}

/// Normalize one path component for policy matching only (never for I/O).
pub fn normalize_component(c: &str) -> String {
    let stripped: String = c.chars().filter(|&ch| !is_ignorable(ch)).collect();
    // NTFS drops trailing dots and spaces: `.git.` and `.git ` open `.git`.
    stripped.trim_end_matches(['.', ' ']).to_ascii_lowercase()
}

/// True for an NTFS 8.3 short name that can alias `.git` (`GIT~1`, `git~2`).
fn is_git_short_name(norm: &str) -> bool {
    norm.strip_prefix("git~")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Classify a validated, `/`-separated relative path.
pub fn classify(rel: &str) -> Classified {
    let comps: Vec<String> = rel
        .split('/')
        .filter(|c| !c.is_empty())
        .map(normalize_component)
        .collect();
    let Some(file) = comps.last() else {
        return Classified {
            class: PathClass::Code,
            reason: "",
        };
    };

    if comps.iter().any(|c| c == ".git" || is_git_short_name(c)) {
        return Classified {
            class: PathClass::Forbidden,
            reason:
                "inside .git (hooks and git config run commands; git never versions these files)",
        };
    }

    let dirs = &comps[..comps.len() - 1];
    for (name, why) in CONTROL_DIRS_ANYWHERE {
        if dirs.iter().any(|d| d == name) {
            return Classified {
                class: PathClass::Control,
                reason: why,
            };
        }
    }
    for (name, why) in CONTROL_FILES_ANYWHERE {
        if file == name {
            return Classified {
                class: PathClass::Control,
                reason: why,
            };
        }
    }
    let joined = comps.join("/");
    for (prefix, why) in CONTROL_ROOT_PREFIXES {
        let hit = if prefix.ends_with('/') {
            joined.starts_with(prefix)
        } else {
            joined == *prefix
        };
        if hit {
            return Classified {
                class: PathClass::Control,
                reason: why,
            };
        }
    }

    let is_requirements = file.starts_with("requirements") && file.ends_with(".txt");
    if MANIFEST_FILES.contains(&file.as_str())
        || is_requirements
        || MANIFEST_SUFFIXES.iter().any(|s| file.ends_with(s))
    {
        return Classified {
            class: PathClass::Manifest,
            reason: "build/dependency manifest — runs code on the next build or install",
        };
    }
    Classified {
        class: PathClass::Code,
        reason: "",
    }
}

/// What a single apply run may write without further confirmation.
#[derive(Debug, Clone, Default)]
pub struct ApplyPolicy {
    allow_control: Option<GlobSet>,
    allow_manifest: Option<GlobSet>,
    /// Modify files that are executable today (their mode is kept).
    pub allow_exec: bool,
    /// Exact relative paths the user confirmed one by one (GUI dialog).
    pub confirmed: Vec<String>,
    /// Directory holding this machine's record of the backups it created
    /// (`restore_last` trusts only those). `None` = the platform data dir or
    /// `$TURBOMERGER_STATE_DIR`; tests point it at a temp dir.
    pub state_dir: Option<std::path::PathBuf>,
}

fn build_globs(patterns: &[String]) -> Result<Option<GlobSet>, String> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        let g = GlobBuilder::new(p.trim_start_matches("./"))
            .case_insensitive(true)
            .literal_separator(true)
            .build()
            .map_err(|e| format!("bad glob '{}': {}", p, e))?;
        b.add(g);
    }
    b.build().map(Some).map_err(|e| e.to_string())
}

impl ApplyPolicy {
    pub fn new(
        allow_control: &[String],
        allow_manifest: &[String],
        allow_exec: bool,
    ) -> Result<Self, String> {
        Ok(Self {
            allow_control: build_globs(allow_control)?,
            allow_manifest: build_globs(allow_manifest)?,
            allow_exec,
            confirmed: Vec::new(),
            state_dir: None,
        })
    }

    /// Keep the trusted-backup record in `dir` (tests, stateless launchers).
    pub fn with_state_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.state_dir = Some(dir.into());
        self
    }

    fn is_confirmed(&self, rel: &str) -> bool {
        self.confirmed.iter().any(|c| c == rel)
    }

    /// True when `rel` (of class `class`) may be written in this run.
    pub fn permits(&self, rel: &str, class: PathClass) -> bool {
        let glob_hit = |set: &Option<GlobSet>| set.as_ref().is_some_and(|s| s.is_match(rel));
        match class {
            PathClass::Code => true,
            PathClass::Forbidden => false,
            PathClass::Manifest => {
                glob_hit(&self.allow_manifest)
                    || glob_hit(&self.allow_control)
                    || self.is_confirmed(rel)
            }
            PathClass::Control => glob_hit(&self.allow_control) || self.is_confirmed(rel),
        }
    }

    /// The flag that would allow `class` — for refusal messages.
    pub fn hint(class: PathClass) -> &'static str {
        match class {
            PathClass::Control => "--allow-control GLOB",
            PathClass::Manifest => "--allow-manifest GLOB",
            _ => "",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(p: &str) -> PathClass {
        classify(p).class
    }

    #[test]
    fn git_internals_and_their_aliases_are_forbidden() {
        for p in [
            ".git/hooks/pre-commit",
            ".git/config",
            ".GIT/hooks/post-checkout",
            ".Git/HEAD",
            ".git./hooks/pre-commit",
            ".git /config",
            ".g\u{200c}it/hooks/pre-push",
            "\u{feff}.git/config",
            "GIT~1/hooks/pre-commit",
            "vendor/lib/.git/config",
            ".git",
        ] {
            assert_eq!(class(p), PathClass::Forbidden, "{p:?}");
        }
        // Lookalikes that are not .git stay ordinary code.
        for p in [
            ".github/README.md",
            "docs/git.md",
            ".gitkeep",
            "src/git~x.rs",
        ] {
            assert_ne!(class(p), PathClass::Forbidden, "{p:?}");
        }
    }

    #[test]
    fn control_paths_are_recognized() {
        for p in [
            ".github/workflows/ci.yml",
            ".GITHUB/Workflows/release.yaml",
            ".github/actions/setup/action.yml",
            ".gitlab-ci.yml",
            ".circleci/config.yml",
            ".vscode/tasks.json",
            ".vscode/settings.json",
            "packages/app/.vscode/launch.json",
            ".idea/workspace.xml",
            ".devcontainer/devcontainer.json",
            ".claude/settings.json",
            ".mcp.json",
            ".envrc",
            "sub/.envrc",
            ".cargo/config.toml",
            ".gitattributes",
            ".gitmodules",
            ".husky/pre-commit",
            ".pre-commit-config.yaml",
            "rust-toolchain.toml",
            "Jenkinsfile",
            ".npmrc",
        ] {
            assert_eq!(class(p), PathClass::Control, "{p:?}");
        }
        // CI files only count at the root.
        assert_eq!(class("docs/examples/.gitlab-ci.yml"), PathClass::Code);
        assert_eq!(class("docs/.github/workflows/ci.yml"), PathClass::Code);
    }

    #[test]
    fn manifests_are_recognized() {
        for p in [
            "build.rs",
            "Cargo.toml",
            "crates/core/Cargo.toml",
            "package.json",
            "pyproject.toml",
            "setup.py",
            "Makefile",
            "CMakeLists.txt",
            "app/build.gradle",
            "app/build.gradle.kts",
            "Dockerfile",
            "requirements-dev.txt",
            "src/App.csproj",
        ] {
            assert_eq!(class(p), PathClass::Manifest, "{p:?}");
        }
        for p in [
            "src/main.rs",
            "README.md",
            "src/build/gen.py",
            "docs/package.md",
        ] {
            assert_eq!(class(p), PathClass::Code, "{p:?}");
        }
    }

    #[test]
    fn policy_globs_and_confirmations() {
        let default = ApplyPolicy::default();
        assert!(default.permits("src/main.rs", PathClass::Code));
        assert!(!default.permits(".vscode/tasks.json", PathClass::Control));
        assert!(!default.permits("build.rs", PathClass::Manifest));
        assert!(!default.permits(".git/config", PathClass::Forbidden));

        let p = ApplyPolicy::new(
            &[".github/workflows/*".to_string()],
            &["build.rs".to_string()],
            false,
        )
        .unwrap();
        assert!(p.permits(".github/workflows/ci.yml", PathClass::Control));
        assert!(!p.permits(".github/workflows/sub/ci.yml", PathClass::Control));
        assert!(!p.permits(".vscode/tasks.json", PathClass::Control));
        assert!(p.permits("build.rs", PathClass::Manifest));
        assert!(!p.permits("Cargo.toml", PathClass::Manifest));

        // Nothing unlocks .git internals.
        let all = ApplyPolicy::new(&["**".to_string()], &["**".to_string()], true).unwrap();
        assert!(!all.permits(".git/hooks/pre-commit", PathClass::Forbidden));

        let mut confirmed = ApplyPolicy::default();
        confirmed
            .confirmed
            .push(".vscode/settings.json".to_string());
        assert!(confirmed.permits(".vscode/settings.json", PathClass::Control));
        assert!(!confirmed.permits(".vscode/tasks.json", PathClass::Control));

        assert!(ApplyPolicy::new(&["[".to_string()], &[], false).is_err());
    }
}
