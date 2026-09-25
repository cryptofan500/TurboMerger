//! Headless command line (N-30, N-31): `turbomerger merge|map|apply|explain|mcp|completions`.
//!
//! v7.7.0 hand-rolled its argv parsing: an unknown flag became the output
//! file name (`merge fx --includ-hidden` wrote a file called
//! `--includ-hidden`), bad numbers were ignored, and `--help`/`--version`
//! fell through to the GUI, which aborts without a display. clap now
//! validates everything; usage errors exit 2 before anything is written.
//!
//! Exit codes (docs/adr/0011-exit-codes.md):
//! - 0 complete — everything found was merged or deliberately excluded
//! - 1 error
//! - 2 usage error
//! - 3 completed, but content was NOT captured (unsupported documents or
//!   photos, too large, unreadable, credential-dense, a failed git section),
//!   or anything at all was skipped under `--fail-on-skip`; apply: some
//!   proposals were held or refused
//!
//! - 4 cancelled (Ctrl-C, `--deadline`, `--on-stall fail`: nothing written)
//!   or partial (`--on-deadline partial`, `--keep-partial`: what was done is
//!   written, the rest is reported as not captured)
//!
//! 5 (verification failed) is reserved for the Phase 5 verifier.

mod report;

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};

use report::{DeadlineAction, ProgressMode, Reporter, ReporterConfig, StallAction};
use tm_core::job::{resolve_job, JobError, MergeOptions};
use tm_core::progress::Tracker;
use tm_core::scanner::{self, SkipEntry, SkipKind};
use tm_core::{security, CancelToken};

pub const EXIT_OK: i32 = 0;
pub const EXIT_ERROR: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_SKIPS: i32 = 3;
/// Cancelled (Ctrl-C, deadline, stall) or a partial output.
pub const EXIT_PARTIAL: i32 = 4;

#[derive(Parser, Debug)]
#[command(
    name = "turbomerger",
    version,
    about = "Merge a codebase or document folder into LLM-ready files; apply LLM replies back safely.",
    after_help = "Run without arguments to open the desktop app. Exit codes: 0 complete, 1 error, 2 usage, 3 completed with content not captured, 4 cancelled or partial."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Merge a folder (or a remote repository) into one or more LLM-ready files.
    Merge(MergeCmd),
    /// Aider-style repo map: ranked signatures within a token budget.
    Map(MapCmd),
    /// Apply the file changes in an LLM reply (a dry run unless --yes).
    Apply(ApplyCmd),
    /// Explain why a path is or is not in the merge.
    Explain(ExplainCmd),
    /// Serve the Model Context Protocol on stdio.
    Mcp(McpCmd),
    /// Print a shell-completion script.
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatArg {
    #[value(alias = "md")]
    Markdown,
    Xml,
    #[value(alias = "claude")]
    Cxml,
    Json,
    #[value(alias = "text", alias = "txt")]
    Plain,
}

impl FormatArg {
    fn as_str(self) -> &'static str {
        match self {
            FormatArg::Markdown => "markdown",
            FormatArg::Xml => "xml",
            FormatArg::Cxml => "cxml",
            FormatArg::Json => "json",
            FormatArg::Plain => "plain",
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderingArg {
    Path,
    EntryFirst,
    ImportantLast,
}

impl OrderingArg {
    fn as_str(self) -> &'static str {
        match self {
            OrderingArg::Path => "path",
            OrderingArg::EntryFirst => "entry-first",
            OrderingArg::ImportantLast => "important-last",
        }
    }
}

/// Scan options shared by merge, map and explain.
#[derive(Args, Debug, Clone, Default)]
pub struct ScanArgs {
    /// Only merge paths matching GLOB (repeatable)
    #[arg(long = "include", value_name = "GLOB")]
    pub include: Vec<String>,
    /// Drop paths matching GLOB (repeatable)
    #[arg(long = "exclude", value_name = "GLOB")]
    pub exclude: Vec<String>,
    /// Ignore .gitignore / .ignore / .turbomergerignore rules
    #[arg(long)]
    pub no_gitignore: bool,
    /// Include dot-files and dot-directories
    #[arg(long)]
    pub include_hidden: bool,
    /// Include Python virtualenvs and caches
    #[arg(long)]
    pub include_venv: bool,
    /// Read settings from this turbomerger.toml instead of <src>/turbomerger.toml
    #[arg(long, value_name = "FILE", env = "TURBOMERGER_CONFIG")]
    pub config: Option<PathBuf>,
    /// Per-file size cap in MB (default 2, or the config file's value)
    #[arg(long, value_name = "MB", env = "TURBOMERGER_MAX_FILE_SIZE",
          value_parser = clap::value_parser!(u64).range(1..=4096))]
    pub max_file_size: Option<u64>,
}

#[derive(Args, Debug)]
pub struct MergeCmd {
    /// Folder to merge, a repository URL (https://…, git@host:…), or gh:owner/repo
    pub src: String,
    /// Output file or directory (default: your Downloads folder)
    pub out: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = FormatArg::Markdown, env = "TURBOMERGER_FORMAT")]
    pub format: FormatArg,
    #[arg(long, value_enum, default_value_t = OrderingArg::Path, env = "TURBOMERGER_ORDERING")]
    pub ordering: OrderingArg,
    /// Split into parts of at most N o200k tokens
    #[arg(long, value_name = "N", env = "TURBOMERGER_MAX_TOKENS",
          value_parser = clap::value_parser!(u64).range(1..))]
    pub max_tokens: Option<u64>,
    #[command(flatten)]
    pub scan: ScanArgs,
    /// Do not redact secrets (the output may then contain credentials)
    #[arg(long)]
    pub no_redact: bool,
    #[arg(long)]
    pub remove_empty_lines: bool,
    #[arg(long)]
    pub truncate_base64: bool,
    /// Signatures only: elide function bodies (tree-sitter)
    #[arg(long)]
    pub compress: bool,
    /// Remove comments (tree-sitter)
    #[arg(long)]
    pub strip_comments: bool,
    /// Append the working-tree diff of the merged files
    #[arg(long)]
    pub git_diff: bool,
    /// Append the last N commit subjects (N defaults to 10 when omitted)
    #[arg(long, value_name = "N", num_args = 0..=1, default_missing_value = "10",
          value_parser = clap::value_parser!(u64).range(1..=10_000))]
    pub git_log: Option<u64>,
    /// Write .claude/skills/<repo>/SKILL.md into the merged folder
    #[arg(long)]
    pub emit_skill: bool,
    /// Put the absolute source path in the output (default: the folder name only)
    #[arg(long)]
    pub show_source_path: bool,
    /// Exit 3 when any file was skipped, not only content that was not captured
    #[arg(long)]
    pub fail_on_skip: bool,
    /// Print nothing on success
    #[arg(short, long)]
    pub quiet: bool,
    /// Progress on stderr
    #[arg(long, value_enum, default_value_t = ProgressMode::Auto, env = "TURBOMERGER_PROGRESS")]
    pub progress: ProgressMode,
    /// Show an ETA once the run is this old (e.g. 60s, 2m)
    #[arg(long, value_name = "DURATION", default_value = "60s", value_parser = duration_arg)]
    pub eta_after: Duration,
    /// Report a stall when nothing moves for this long
    #[arg(long, value_name = "DURATION", default_value = "10s", value_parser = duration_arg)]
    pub stall_after: Duration,
    /// What to do on a stall
    #[arg(long, value_enum, default_value_t = StallAction::Wait)]
    pub on_stall: StallAction,
    /// Time limit for the whole run (e.g. 10m, 1h30m)
    #[arg(long, value_name = "DURATION", value_parser = duration_arg)]
    pub deadline: Option<Duration>,
    /// What to do at the deadline
    #[arg(long, value_enum, default_value_t = DeadlineAction::Cancel, requires = "deadline")]
    pub on_deadline: DeadlineAction,
    /// On Ctrl-C, write what is done (exit 4) instead of nothing
    #[arg(long)]
    pub keep_partial: bool,
}

fn duration_arg(s: &str) -> Result<Duration, String> {
    tm_core::progress::parse_duration(s)
}

#[derive(Args, Debug)]
pub struct MapCmd {
    /// Folder, repository URL, or gh:owner/repo
    pub src: String,
    /// Write the map here instead of stdout
    pub out: Option<PathBuf>,
    /// Token budget
    #[arg(long, visible_alias = "max-tokens", value_name = "N", default_value_t = 1024,
          value_parser = clap::value_parser!(u64).range(64..=1_000_000))]
    pub tokens: u64,
    #[command(flatten)]
    pub scan: ScanArgs,
}

#[derive(Args, Debug)]
pub struct ApplyCmd {
    /// Folder the reply's paths are relative to
    pub root: PathBuf,
    /// The LLM reply (Markdown with `## path` + fenced blocks, cxml, or unified diffs)
    #[arg(
        long,
        value_name = "FILE",
        required_unless_present = "restore",
        conflicts_with = "restore"
    )]
    pub from: Option<PathBuf>,
    /// Write the changes (default: dry run)
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Undo the most recent apply from its backup
    #[arg(long)]
    pub restore: bool,
    /// Allow control files matching GLOB (CI, hooks managers, editor/agent settings; repeatable)
    #[arg(long, value_name = "GLOB")]
    pub allow_control: Vec<String>,
    /// Allow build/dependency manifests matching GLOB (build.rs, package.json…; repeatable)
    #[arg(long, value_name = "GLOB")]
    pub allow_manifest: Vec<String>,
    /// Allow modifying files that are executable today (their mode is kept)
    #[arg(long)]
    pub allow_exec: bool,
}

#[derive(Args, Debug)]
pub struct ExplainCmd {
    /// Folder that would be merged
    pub root: PathBuf,
    /// Path to explain (relative to the folder, or absolute inside it)
    pub path: PathBuf,
    #[command(flatten)]
    pub scan: ScanArgs,
}

#[derive(Args, Debug, Default, Clone)]
pub struct McpCmd {
    /// Folder MCP clients may pack or map (repeatable). Default: the current
    /// directory, unless it is / or your home directory.
    #[arg(long, value_name = "DIR")]
    pub root: Vec<PathBuf>,
    /// Where pack_directory writes its outputs (default: an app data folder)
    #[arg(long, value_name = "DIR")]
    pub output_dir: Option<PathBuf>,
    /// Let clients pack remote repositories (https://…, gh:owner/repo)
    #[arg(long)]
    pub allow_remote: bool,
}

/// Parse and run a command line. `None` = no arguments (the caller opens
/// the desktop app or prints help; see `launch_gui_or_help`).
pub fn run(argv: &[OsString]) -> Option<i32> {
    if argv.len() <= 1 {
        return None;
    }
    let cli = match Cli::try_parse_from(argv) {
        Ok(c) => c,
        Err(e) => {
            // --help / --version land here too (exit 0, stdout).
            let _ = e.print();
            return Some(e.exit_code());
        }
    };
    Some(match cli.command {
        Command::Merge(m) => run_merge(m),
        Command::Map(m) => run_map(m),
        Command::Apply(a) => run_apply(a),
        Command::Explain(e) => run_explain(e),
        Command::Mcp(m) => tm_mcp::run_mcp(tm_mcp::McpConfig {
            roots: m.root,
            output_dir: m.output_dir,
            allow_remote: m.allow_remote,
        }),
        Command::Completions { shell } => {
            clap_complete::generate(
                shell,
                &mut Cli::command(),
                "turbomerger",
                &mut std::io::stdout(),
            );
            EXIT_OK
        }
    })
}

/// The subcommand names, for shells that forward to this CLI (the desktop
/// binary runs `turbomerger-gui merge …` through here for compatibility).
pub fn is_subcommand(arg: &std::ffi::OsStr) -> bool {
    let Some(a) = arg.to_str() else {
        return false;
    };
    a.starts_with('-')
        || Cli::command()
            .get_subcommands()
            .any(|c| c.get_name() == a || c.get_all_aliases().any(|al| al == a))
        || a == "help"
}

/// `turbomerger` with no arguments: open the desktop app when one is
/// installed next to this binary (or on PATH) and a display is available;
/// otherwise print the help. Never blocks on the GUI.
pub fn launch_gui_or_help() -> i32 {
    let name = if cfg!(windows) {
        "turbomerger-gui.exe"
    } else {
        "turbomerger-gui"
    };
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join(name)))
        .filter(|p| p.is_file());
    let on_path = || {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|d| d.join(name))
                .find(|p| p.is_file())
        })
    };
    let has_display = cfg!(any(windows, target_os = "macos"))
        || std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty())
        || std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty());
    if has_display {
        if let Some(gui) = beside.or_else(on_path) {
            let spawned = std::process::Command::new(&gui)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            if spawned.is_ok() {
                return EXIT_OK;
            }
        }
    }
    let _ = Cli::command().print_help();
    eprintln!();
    EXIT_USAGE
}

/// A local path, or an explicit remote reference cloned into a temp dir
/// (announced on stderr).
fn resolve_source(src: &str) -> Result<tm_core::job::ResolvedSource, String> {
    tm_core::job::resolve_source(src, |url| eprintln!("cloning {} (shallow)...", url))
}

fn merge_options(src_root: String, scan: &ScanArgs) -> MergeOptions {
    let mut o = MergeOptions::for_folder(src_root);
    o.include_venv = scan.include_venv;
    o.respect_gitignore = !scan.no_gitignore;
    o.include_hidden = scan.include_hidden;
    o.include_globs = scan.include.clone();
    o.exclude_globs = scan.exclude.clone();
    o.config_path = scan
        .config
        .as_ref()
        .map(|p| p.to_string_lossy().to_string());
    o.max_file_size_mb = scan.max_file_size;
    o
}

/// The exit code a finished merge earns (see the module docs).
pub fn merge_exit_code(files_merged: usize, skips: &[&SkipEntry], fail_on_skip: bool) -> i32 {
    let not_captured = skips.iter().any(|s| s.kind == SkipKind::NotCaptured);
    let file_skipped = skips.iter().any(|s| {
        matches!(
            s.kind,
            SkipKind::Excluded | SkipKind::Binary | SkipKind::NotCaptured
        )
    });
    if files_merged == 0 || not_captured || (fail_on_skip && file_skipped) {
        EXIT_SKIPS
    } else {
        EXIT_OK
    }
}

fn run_merge(a: MergeCmd) -> i32 {
    let source = match resolve_source(&a.src) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {}", e);
            return EXIT_ERROR;
        }
    };
    let remote_label = source.remote_label.clone();
    let mut options = merge_options(source.root.clone(), &a.scan);
    options.output_path = a.out.as_ref().map(|p| p.to_string_lossy().to_string());
    options.redact_secrets = !a.no_redact;
    options.format = Some(a.format.as_str().to_string());
    options.ordering = Some(a.ordering.as_str().to_string());
    options.max_tokens = a.max_tokens.map(|t| t as usize);
    options.remove_empty_lines = a.remove_empty_lines;
    options.truncate_base64 = a.truncate_base64;
    options.compress = a.compress;
    options.strip_comments = a.strip_comments;
    options.git_diff = a.git_diff;
    options.git_log_count = a.git_log.unwrap_or(0) as usize;
    options.emit_skill = a.emit_skill;
    options.show_source_path = a.show_source_path;
    options.remote = remote_label.is_some();
    options.source_label = remote_label;

    // Ctrl-C cancels the job (temp files are cleaned up); a second one quits.
    let token = CancelToken::new();
    let interrupted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let token = token.clone();
        let keep = a.keep_partial;
        let pressed = interrupted.clone();
        let _ = ctrlc::set_handler(move || {
            if pressed.swap(true, std::sync::atomic::Ordering::Relaxed) {
                std::process::exit(130);
            }
            if keep {
                token.finish_early();
            } else {
                token.cancel();
            }
        });
    }
    let tracker = Arc::new(Tracker::new(a.eta_after, a.stall_after));
    let reporter = Reporter::start(
        tracker.clone(),
        token.clone(),
        ReporterConfig {
            mode: a.progress,
            quiet: a.quiet,
            deadline: a.deadline,
            on_deadline: a.on_deadline,
            on_stall: a.on_stall,
        },
    );
    let run = tm_core::job::run_merge(&options, None, &token, &|p| tracker.update(&p));
    let stopped_by = reporter.finish().or_else(|| {
        interrupted
            .load(std::sync::atomic::Ordering::Relaxed)
            .then_some("Ctrl-C")
    });

    let run = match run {
        Ok(r) => r,
        Err(JobError::Cancelled) => {
            eprintln!(
                "cancelled{} — nothing was written",
                stopped_by.map(|w| format!(" ({})", w)).unwrap_or_default()
            );
            return EXIT_PARTIAL;
        }
        Err(JobError::Empty) => {
            eprintln!("no files found");
            return EXIT_ERROR;
        }
        Err(JobError::Invalid(e)) => {
            eprintln!("error: {}", e);
            return EXIT_ERROR;
        }
        Err(JobError::Scan(e)) => {
            eprintln!("scan failed: {}", e);
            return EXIT_ERROR;
        }
        Err(JobError::Merge(e)) => {
            eprintln!("merge failed: {}", e);
            return EXIT_ERROR;
        }
    };
    let o = &run.outcome;
    let all: Vec<&SkipEntry> = run.all_skips().collect();
    let not_captured = all
        .iter()
        .filter(|s| s.kind == SkipKind::NotCaptured)
        .count();
    let code = if o.partial {
        EXIT_PARTIAL
    } else {
        merge_exit_code(o.files_processed, &all, a.fail_on_skip)
    };
    if !a.quiet || code != EXIT_OK {
        // Aggregate, non-secret output only.
        println!(
            "merged={} scan_skipped={} merge_skipped={} redacted={} tokens_o200k={} parts={} not_captured={}",
            o.files_processed,
            run.scan_skipped.len(),
            o.files_skipped,
            o.secrets_redacted,
            o.tokens_o200k,
            o.outputs.len(),
            not_captured
        );
        for p in &o.outputs {
            println!("out={}", p.display());
        }
        if let Some(s) = &o.skill {
            println!("skill={}", s.display());
        }
    }
    for note in &o.notes {
        eprintln!("warning: {}", note);
    }
    if o.partial {
        eprintln!(
            "warning: partial output — files not processed before the {} are listed as not captured",
            stopped_by.unwrap_or("stop")
        );
    } else if o.files_processed == 0 {
        eprintln!("warning: nothing was merged — the output holds only the report of what was skipped and why");
    } else if not_captured > 0 {
        eprintln!(
            "warning: {} input(s) were not captured — see \"Not captured\" in the Merge Report",
            not_captured
        );
    }
    code
}

fn run_map(a: MapCmd) -> i32 {
    let source = match resolve_source(&a.src) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {}", e);
            return EXIT_ERROR;
        }
    };
    let mut options = merge_options(source.root.clone(), &a.scan);
    options.include_tree = false;
    let job = match resolve_job(&options) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("error: {}", e);
            return EXIT_ERROR;
        }
    };
    let scan = match scanner::scan_text_files(&job.root, &job.scan_options) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("scan failed: {}", e);
            return EXIT_ERROR;
        }
    };
    let map = tm_core::repomap::build_repo_map(&job.root, &scan.files, a.tokens as usize);
    match a.out {
        Some(out) => {
            if let Err(e) = std::fs::write(&out, &map) {
                eprintln!("write failed: {}", e);
                return EXIT_ERROR;
            }
            println!("out={}", out.display());
        }
        None => print!("{}", map),
    }
    EXIT_OK
}

/// Headless apply/restore. Prints paths + counts only (never content —
/// pasted replies can embed secrets).
fn run_apply(a: ApplyCmd) -> i32 {
    let root = match security::validate_and_canonicalize(&a.root.to_string_lossy()) {
        Ok(r) if r.is_dir() => r,
        Ok(_) => {
            eprintln!("error: root is not a folder");
            return EXIT_ERROR;
        }
        Err(e) => {
            eprintln!("error: {}", e);
            return EXIT_ERROR;
        }
    };
    let policy = match tm_core::applyback::ApplyPolicy::new(
        &a.allow_control,
        &a.allow_manifest,
        a.allow_exec,
    ) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {}", e);
            return EXIT_USAGE;
        }
    };

    if a.restore {
        return match tm_core::applyback::restore_last(&root, &policy) {
            Ok(r) => {
                for p in &r.restored {
                    println!("restored {}", p);
                }
                for p in &r.deleted {
                    println!("deleted  {}", p);
                }
                for f in &r.skipped {
                    println!("SKIP     {} — {}", f.rel_path, f.reason);
                }
                println!("from={}", r.backup_dir);
                if r.skipped.is_empty() {
                    EXIT_OK
                } else {
                    EXIT_SKIPS
                }
            }
            Err(e) => {
                eprintln!("restore failed: {}", e);
                EXIT_ERROR
            }
        };
    }

    let from = a.from.expect("clap requires --from without --restore");
    let reply = match std::fs::read_to_string(&from) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: cannot read {}: {}", from.display(), e);
            return EXIT_ERROR;
        }
    };
    let changes = tm_core::applyback::parse_reply(&reply);
    if changes.is_empty() {
        eprintln!("no file changes recognized in {}", from.display());
        return EXIT_ERROR;
    }
    let built = match tm_core::applyback::build_preview(&root, &changes, &policy) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: {}", e);
            return EXIT_ERROR;
        }
    };
    let mut held_or_refused = 0usize;
    for f in &built.preview.files {
        let extra: Vec<&str> = [f.mode_note.as_str(), f.encoding.as_str()]
            .into_iter()
            .filter(|s| !s.is_empty() && *s != "UTF-8")
            .collect();
        let extra = if extra.is_empty() {
            String::new()
        } else {
            format!("  ({})", extra.join("; "))
        };
        if f.identical {
            println!("same   {} (already matches disk)", f.rel_path);
        } else if f.ok && !f.needs_confirm {
            println!(
                "{:6} {} +{} -{}{}",
                f.action, f.rel_path, f.adds, f.dels, extra
            );
        } else if f.ok {
            held_or_refused += 1;
            println!("HOLD   {} — {}", f.rel_path, f.note);
        } else {
            held_or_refused += 1;
            println!("SKIP   {} — {}", f.rel_path, f.note);
        }
    }
    let appliable: Vec<tm_core::applyback::ReadyFile> = built
        .ready
        .into_iter()
        .filter(|f| !f.needs_confirm)
        .collect();
    let skips_exit = if held_or_refused > 0 {
        EXIT_SKIPS
    } else {
        EXIT_OK
    };
    if appliable.is_empty() {
        println!("nothing to apply");
        return skips_exit;
    }
    if !a.yes {
        println!(
            "dry-run: {} file(s) would be written — pass --yes to apply (backups are taken)",
            appliable.len()
        );
        return skips_exit;
    }
    match tm_core::applyback::apply_files(&root, &appliable, &policy) {
        Ok(o) => {
            for p in &o.applied {
                println!("applied {}", p);
            }
            for f in &o.failed {
                eprintln!("failed  {} — {}", f.rel_path, f.reason);
            }
            if let Some(b) = &o.backup_dir {
                println!("backup={}", b);
                println!("undo: turbomerger apply \"{}\" --restore", root.display());
            }
            if !o.failed.is_empty() {
                EXIT_ERROR
            } else {
                skips_exit
            }
        }
        Err(e) => {
            eprintln!("apply failed: {}", e);
            EXIT_ERROR
        }
    }
}

/// `turbomerger explain <root> <path>` — the decision chain for one path
/// (N-01 d): merged, skipped (why), inside a directory that is not
/// scanned, or hidden by an ignore rule.
fn run_explain(a: ExplainCmd) -> i32 {
    let options = merge_options(a.root.to_string_lossy().to_string(), &a.scan);
    let job = match resolve_job(&options) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("error: {}", e);
            return EXIT_ERROR;
        }
    };
    // Resolve only the directories: the last component is explained as named
    // (canonicalizing it would explain a symlink's target instead).
    let rel = if a.path.is_absolute() {
        let parent = a.path.parent().and_then(|p| std::fs::canonicalize(p).ok());
        let joined = match (parent, a.path.file_name()) {
            (Some(p), Some(name)) => p.join(name),
            _ => a.path.clone(),
        };
        match joined.strip_prefix(&job.root) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => {
                eprintln!(
                    "error: {} is not inside {}",
                    a.path.display(),
                    job.root.display()
                );
                return EXIT_USAGE;
            }
        }
    } else {
        let r = a.path.to_string_lossy().replace('\\', "/");
        r.trim_start_matches("./").trim_end_matches('/').to_string()
    };
    let target = job.root.join(&rel);
    let scan = match scanner::scan_text_files(&job.root, &job.scan_options) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("scan failed: {}", e);
            return EXIT_ERROR;
        }
    };
    println!("{}", rel);
    if scan
        .files
        .iter()
        .any(|f| scanner::relative_display(&job.root, f) == rel)
    {
        println!(
            "  merged (merge-time checks — binary content, credential density — may still drop it)"
        );
        return EXIT_OK;
    }
    if let Some(s) = scan
        .skipped
        .iter()
        .find(|s| s.path == rel || s.path == format!("{}/", rel))
    {
        println!("  not merged: {} [{}]", s.reason, kind_word(s.kind));
        return EXIT_OK;
    }
    if let Some(s) = scan
        .skipped
        .iter()
        .find(|s| s.kind == SkipKind::PrunedDir && rel.starts_with(&s.path))
    {
        println!("  not merged: inside {} — {}", s.path, s.reason);
        return EXIT_OK;
    }
    let ignored = scan
        .skipped
        .iter()
        .filter(|s| s.kind == SkipKind::IgnoredByRule)
        .collect::<Vec<_>>();
    if std::fs::symlink_metadata(&target).is_err() {
        println!("  does not exist");
    } else if !ignored.is_empty() {
        println!("  not merged: hidden by an ignore rule or glob. Rules in effect:");
        for s in ignored {
            println!("    {} — {}", s.path, s.reason);
        }
    } else {
        println!("  not merged (no rule recorded — please report this)");
    }
    EXIT_OK
}

fn kind_word(k: SkipKind) -> &'static str {
    match k {
        SkipKind::Excluded => "excluded on purpose",
        SkipKind::Binary => "binary",
        SkipKind::NotCaptured => "NOT captured",
        SkipKind::PrunedDir => "directory not scanned",
        SkipKind::IgnoredByRule => "ignore rule",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        let mut v = vec!["turbomerger"];
        v.extend_from_slice(args);
        Cli::try_parse_from(v)
    }

    #[test]
    fn typos_and_bad_numbers_are_usage_errors() {
        // v1 A.3: the typo used to become the output file name.
        let e = parse(&["merge", "fx", "--includ-hidden"]).unwrap_err();
        assert_eq!(e.exit_code(), EXIT_USAGE);
        let e = parse(&["merge", "fx", "out.md", "--max-tokens", "abc"]).unwrap_err();
        assert_eq!(e.exit_code(), EXIT_USAGE);
        let e = parse(&["merge", "fx", "out.md", "--max-tokens", "0"]).unwrap_err();
        assert_eq!(e.exit_code(), EXIT_USAGE);
        let e = parse(&["merge", "fx", "--format", "yaml"]).unwrap_err();
        assert_eq!(e.exit_code(), EXIT_USAGE);
        assert!(
            parse(&["merge", "a", "b", "c"]).is_err(),
            "third positional"
        );
    }

    #[test]
    fn help_and_version_do_not_need_the_gui() {
        let e = parse(&["--version"]).unwrap_err();
        assert_eq!(e.kind(), clap::error::ErrorKind::DisplayVersion);
        assert_eq!(e.exit_code(), 0);
        let e = parse(&["merge", "--help"]).unwrap_err();
        assert_eq!(e.kind(), clap::error::ErrorKind::DisplayHelp);
    }

    #[test]
    fn flags_parse_strictly() {
        let Cli {
            command: Command::Merge(m),
        } = parse(&[
            "merge",
            "src",
            "out.md",
            "--format",
            "md",
            "--git-log",
            "--compress",
            "--max-tokens",
            "3000",
            "--exclude",
            "docs/**",
        ])
        .unwrap()
        else {
            panic!("merge")
        };
        assert_eq!(m.format, FormatArg::Markdown);
        assert_eq!(m.git_log, Some(10), "bare --git-log means 10");
        assert!(m.compress);
        assert_eq!(m.max_tokens, Some(3000));
        assert_eq!(m.scan.exclude, vec!["docs/**".to_string()]);
        // --git-log no longer swallows the next token.
        assert!(parse(&["merge", "src", "--git-log", "out.md"]).is_err());

        let Cli {
            command: Command::Apply(a),
        } = parse(&[
            "apply",
            "root",
            "--from",
            "r.md",
            "--allow-control",
            ".github/**",
        ])
        .unwrap()
        else {
            panic!("apply")
        };
        assert_eq!(a.allow_control, vec![".github/**".to_string()]);
        assert!(
            parse(&["apply", "root"]).is_err(),
            "--from or --restore required"
        );
        assert!(parse(&["apply", "root", "--restore", "--from", "x"]).is_err());
    }

    #[test]
    fn exit_codes_follow_the_contract() {
        let nc = SkipEntry::new("paper.pdf", "document", SkipKind::NotCaptured);
        let ex = SkipEntry::new(".env", "env file", SkipKind::Excluded);
        let pr = SkipEntry::new(".git/", "vcs", SkipKind::PrunedDir);
        assert_eq!(merge_exit_code(3, &[&ex, &pr], false), EXIT_OK);
        assert_eq!(merge_exit_code(3, &[&ex, &pr], true), EXIT_SKIPS);
        assert_eq!(merge_exit_code(3, &[&pr], true), EXIT_OK);
        assert_eq!(merge_exit_code(3, &[&nc], false), EXIT_SKIPS);
        assert_eq!(merge_exit_code(0, &[&ex], false), EXIT_SKIPS);
    }
}
