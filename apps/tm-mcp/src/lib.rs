//! MCP server (T3-2): `turbomerger mcp` speaks the Model Context Protocol
//! over stdio so Claude Desktop/Code and other MCP clients can pull repo
//! context on demand. Built on the official Rust SDK, `rmcp` (N-19, ADR
//! 0014): protocol-version negotiation, progress notifications and
//! request cancellation come from it.
//!
//! Tools: pack_directory (full merge → file, summary returned),
//! repo_map (map text returned inline), read_output / grep_output
//! (sliced access to previously produced outputs — deliberately NOT
//! general-purpose file readers).
//!
//! Safety (N-18): an MCP client is an agent that may be prompt-injected.
//! - Only folders under the server's `--root` directories can be packed or
//!   mapped (default: the working directory, never `/` or the home folder).
//! - Outputs always go to the managed outputs directory; the client's
//!   `output` argument is only a file-name hint. v7.7.0 wrote wherever the
//!   client said (repro: `…/.bashrc_probe`).
//! - read_output / grep_output serve files inside that directory only.
//! - Remote repositories need `--allow-remote`.
//! - Secret redaction is forced on.
//!
//! Long calls report `notifications/progress` when the client sends a
//! progress token, and `notifications/cancelled` stops them (nothing is
//! written for a cancelled pack).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ProgressNotificationParam,
    ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, ServiceExt};
use serde_json::{json, Value};

use tm_core::job::{run_merge, JobError, MergeOptions, Progress, ResolvedSource, Stage};
use tm_core::CancelToken;

const READ_DEFAULT_LINES: usize = 200;
const READ_MAX_LINES: usize = 1000;
const READ_MAX_BYTES: usize = 200 * 1024;
const GREP_MAX_MATCHES: usize = 100;
/// At most this often per call; the last report is always sent.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

/// How the server was started (`turbomerger mcp --root … --output-dir …
/// --allow-remote`).
#[derive(Debug, Clone, Default)]
pub struct McpConfig {
    /// Folders clients may pack or map. Empty: the working directory, unless
    /// it is `/` or the home folder.
    pub roots: Vec<PathBuf>,
    /// Where pack_directory writes (default: an app data folder).
    pub output_dir: Option<PathBuf>,
    /// Let clients pack remote repositories.
    pub allow_remote: bool,
}

/// What this server instance may touch.
#[derive(Debug, Clone)]
pub struct McpServer {
    /// Canonical folders clients may pack/map (empty = nothing allowed).
    roots: Vec<PathBuf>,
    /// Canonical managed outputs directory.
    output_dir: PathBuf,
    allow_remote: bool,
}

impl McpServer {
    /// Build from the command line: `--root` dirs (default: the working
    /// directory unless it is `/` or the home folder) and `--output-dir`
    /// (default: `<data dir>/com.turbomerger.app/mcp-outputs`).
    pub fn from_config(args: &McpConfig) -> Result<McpServer, String> {
        let mut roots = Vec::new();
        let requested: Vec<PathBuf> = if args.roots.is_empty() {
            let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
            let home = dirs::home_dir();
            let too_broad = cwd.parent().is_none() || home.as_deref() == Some(cwd.as_path());
            if too_broad {
                Vec::new()
            } else {
                vec![cwd]
            }
        } else {
            args.roots.clone()
        };
        for r in requested {
            let c = tm_core::security::validate_and_canonicalize(&r.to_string_lossy())
                .map_err(|e| format!("--root {}: {}", r.display(), e))?;
            roots.push(c);
        }
        let output_dir = match &args.output_dir {
            Some(d) => d.clone(),
            None => dirs::data_local_dir()
                .ok_or("no data directory; pass --output-dir")?
                .join("com.turbomerger.app")
                .join("mcp-outputs"),
        };
        std::fs::create_dir_all(&output_dir)
            .map_err(|e| format!("cannot create {}: {}", output_dir.display(), e))?;
        let output_dir = dunce::canonicalize(&output_dir).map_err(|e| e.to_string())?;
        Ok(McpServer {
            roots,
            output_dir,
            allow_remote: args.allow_remote,
        })
    }

    /// Resolve a client-supplied source: an explicit remote reference (when
    /// allowed) or a folder under one of the roots.
    fn resolve_source(&self, src: &str) -> Result<ResolvedSource, String> {
        if tm_core::remote::parse_remote_explicit(src).is_some() && !Path::new(src).exists() {
            if !self.allow_remote {
                return Err(
                    "remote repositories are disabled for this server (start it with --allow-remote)"
                        .into(),
                );
            }
            return tm_core::job::resolve_source(src, |url| {
                eprintln!("cloning {} (shallow)...", url)
            });
        }
        if self.roots.is_empty() {
            return Err(
                "no folder is shared with MCP clients — start the server with `turbomerger mcp --root <DIR>`"
                    .into(),
            );
        }
        let canon = tm_core::security::validate_and_canonicalize(src)
            .map_err(|e| format!("{}: {}", src, e))?;
        if !self.roots.iter().any(|r| canon.starts_with(r)) {
            return Err(format!(
                "{} is outside the folders shared with this server ({})",
                src,
                self.roots
                    .iter()
                    .map(|r| r.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        Ok(ResolvedSource {
            root: canon.to_string_lossy().to_string(),
            checkout: None,
            remote_label: None,
        })
    }

    /// A path inside the managed outputs directory, or an error.
    fn checked_output_path(&self, path: &str) -> Result<PathBuf, String> {
        let p = Path::new(path);
        let candidate = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.output_dir.join(p)
        };
        let canon = dunce::canonicalize(&candidate)
            .map_err(|_| format!("not a TurboMerger output: {}", path))?;
        if !canon.starts_with(&self.output_dir) || !canon.is_file() {
            return Err(format!(
                "read_output/grep_output only serve files in {}",
                self.output_dir.display()
            ));
        }
        Ok(canon)
    }

    /// Run one tool call to completion (blocking). Tool-level failures are
    /// `Err` text the client sees as an `isError` result.
    pub fn call(
        &self,
        name: &str,
        args: &Value,
        cancel: &CancelToken,
        progress: &(dyn Fn(Progress) + Sync),
    ) -> Result<String, String> {
        match name {
            "pack_directory" => self.tool_pack_directory(args, cancel, progress),
            "repo_map" => self.tool_repo_map(args),
            "read_output" => self.tool_read_output(args),
            "grep_output" => self.tool_grep_output(args),
            other => Err(format!("unknown tool: {}", other)),
        }
    }
}

/// Blocking stdio server. Returns the process exit code.
pub fn run_mcp(args: McpConfig) -> i32 {
    let server = match McpServer::from_config(&args) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };
    if server.roots.is_empty() {
        eprintln!("warning: no --root given and the working directory is / or your home folder; pack_directory and repo_map will refuse local paths");
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };
    runtime.block_on(async move {
        let service = match server.serve(rmcp::transport::stdio()).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: MCP handshake failed: {}", e);
                return 1;
            }
        };
        match service.waiting().await {
            Ok(_) => 0,
            Err(e) => {
                eprintln!("error: {}", e);
                1
            }
        }
    })
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "turbomerger",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Pack a folder into LLM-ready files with pack_directory (secrets are always redacted), \
                 then read or search the output with read_output / grep_output. repo_map gives a ranked \
                 overview within a token budget.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let name = request.name.to_string();
        let args = Value::Object(request.arguments.unwrap_or_default());
        let progress_token = request
            .meta
            .as_ref()
            .and_then(|m| m.get_progress_token())
            .or_else(|| context.meta.get_progress_token());

        // notifications/cancelled → the job's cancel token.
        let token = CancelToken::new();
        let watcher = {
            let (ct, token) = (context.ct.clone(), token.clone());
            tokio::spawn(async move {
                ct.cancelled().await;
                token.cancel();
            })
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Progress>();
        let server = self.clone();
        let job_token = token.clone();
        let mut work = tokio::task::spawn_blocking(move || {
            server.call(&name, &args, &job_token, &|p| {
                let _ = tx.send(p);
            })
        });

        let mut last_sent: Option<Instant> = None;
        let mut pending: Option<Progress> = None;
        let result = loop {
            tokio::select! {
                r = &mut work => break r,
                Some(p) = rx.recv() => {
                    let Some(tok) = &progress_token else { continue };
                    if last_sent.is_some_and(|t| t.elapsed() < PROGRESS_EVERY) {
                        pending = Some(p);
                        continue;
                    }
                    last_sent = Some(Instant::now());
                    pending = None;
                    let _ = context.peer.notify_progress(progress_param(tok.clone(), &p)).await;
                }
            }
        };
        if let (Some(tok), Some(p)) = (&progress_token, pending) {
            let _ = context
                .peer
                .notify_progress(progress_param(tok.clone(), &p))
                .await;
        }
        watcher.abort();

        let outcome = result.map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(match outcome {
            Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
            Err(e) => CallToolResult::error(vec![ContentBlock::text(e)]),
        }
        .into())
    }
}

fn progress_param(token: rmcp::model::ProgressToken, p: &Progress) -> ProgressNotificationParam {
    let what = match p.stage {
        Stage::Scan => format!("scanning ({} entries)", p.done),
        Stage::Classify => "classifying files".to_string(),
        Stage::Merge => format!("merging {}", p.current),
    };
    let mut param = ProgressNotificationParam::new(token, p.done as f64).with_message(what);
    if p.total > 0 {
        param = param.with_total(p.total as f64);
    }
    param
}

fn schema(v: Value) -> Arc<JsonObject> {
    match v {
        Value::Object(m) => Arc::new(m),
        _ => unreachable!("tool schemas are objects"),
    }
}

fn tools() -> Vec<Tool> {
    vec![
        Tool::new(
            "pack_directory",
            "Merge a folder under the server's shared roots (or, with --allow-remote, a repo URL / gh:owner/repo) into one LLM-ready file (gitignore-aware, secrets always redacted). Returns a summary plus the output path(s); use read_output/grep_output to access the content.",
            schema(json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Folder under the server's shared roots, or (with --allow-remote) a repo URL / gh:owner/repo" },
                    "output": { "type": "string", "description": "Output file NAME (written to the server's outputs folder)" },
                    "format": { "type": "string", "enum": ["markdown", "xml", "cxml", "json", "plain"] },
                    "ordering": { "type": "string", "enum": ["path", "entry-first", "important-last"] },
                    "max_tokens": { "type": "integer", "description": "Split output above this o200k budget" },
                    "compress": { "type": "boolean", "description": "Signatures-only: elide function bodies" },
                    "strip_comments": { "type": "boolean" },
                    "git_diff": { "type": "boolean", "description": "Append git diff HEAD section" },
                    "git_log": { "type": "integer", "description": "Append git log of last N commits" },
                    "emit_skill": { "type": "boolean", "description": "Write .claude/skills/<repo>/SKILL.md into the repo" },
                    "include_hidden": { "type": "boolean" },
                    "no_gitignore": { "type": "boolean", "description": "Ignore .gitignore rules (default false)" }
                },
                "required": ["path"]
            })),
        ),
        Tool::new(
            "repo_map",
            "Aider-style repo map: ranked file signatures (tree-sitter tags + PageRank) rendered to a token budget. The best first look at a repo that won't fit in context.",
            schema(json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Folder under the server's shared roots, or (with --allow-remote) a repo URL / gh:owner/repo" },
                    "tokens": { "type": "integer", "description": "Token budget (default 1024)" }
                },
                "required": ["path"]
            })),
        ),
        Tool::new(
            "read_output",
            "Read a slice of a TurboMerger output (a path returned by pack_directory) by line offset/limit.",
            schema(json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": "integer", "description": "0-based first line (default 0)" },
                    "limit": { "type": "integer", "description": "Lines to return (default 200, max 1000)" }
                },
                "required": ["path"]
            })),
        ),
        Tool::new(
            "grep_output",
            "Regex-search a TurboMerger output (a path returned by pack_directory); returns matching lines with line numbers (max 100).",
            schema(json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "pattern": { "type": "string", "description": "Rust regex" },
                    "max_matches": { "type": "integer" }
                },
                "required": ["path", "pattern"]
            })),
        ),
    ]
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}
fn arg_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}
fn arg_usize(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(|v| v.as_u64()).map(|n| n as usize)
}

impl McpServer {
    fn tool_pack_directory(
        &self,
        args: &Value,
        cancel: &CancelToken,
        progress: &(dyn Fn(Progress) + Sync),
    ) -> Result<String, String> {
        let src = arg_str(args, "path").ok_or("path is required")?;
        let source = self.resolve_source(src)?;
        let format = arg_str(args, "format").map(|s| s.to_string());
        // The client names the file at most; the directory is always ours.
        let output_path = arg_str(args, "output").map(|hint| {
            let base = Path::new(hint)
                .file_name()
                .map(|n| tm_core::security::sanitize_filename(&n.to_string_lossy()))
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| "merged".into());
            self.output_dir.join(base).to_string_lossy().to_string()
        });
        let mut options = MergeOptions::for_folder(source.root.clone());
        options.output_path =
            Some(output_path.unwrap_or_else(|| self.output_dir.to_string_lossy().to_string()));
        options.respect_gitignore = !arg_bool(args, "no_gitignore");
        options.include_hidden = arg_bool(args, "include_hidden");
        // Forced: an MCP client must never produce an unredacted dump.
        options.redact_secrets = true;
        options.format = format;
        options.ordering = arg_str(args, "ordering").map(|s| s.to_string());
        options.max_tokens = arg_usize(args, "max_tokens");
        options.compress = arg_bool(args, "compress");
        options.strip_comments = arg_bool(args, "strip_comments");
        options.git_diff = arg_bool(args, "git_diff");
        options.git_log_count = arg_usize(args, "git_log").unwrap_or(0);
        options.emit_skill = arg_bool(args, "emit_skill");
        options.remote = source.remote_label.is_some();
        options.source_label = source.remote_label.clone();
        let job = tm_core::job::resolve_job(&options)?;
        if !job.output_path.starts_with(&self.output_dir) {
            return Err("output must stay in the outputs folder".into());
        }
        let run = run_merge(&options, None, cancel, progress).map_err(|e| match e {
            JobError::Scan(m) => format!("scan failed: {}", m),
            JobError::Merge(m) => format!("merge failed: {}", m),
            JobError::Cancelled => "cancelled — nothing was written".to_string(),
            other => other.to_string(),
        })?;
        let o = &run.outcome;
        let mut text = format!(
            "merged={} skipped={} not_captured={} secrets_redacted={} tokens_o200k={} (~{} Claude est.) parts={}\n",
            o.files_processed,
            o.files_skipped + run.scan_skipped.len(),
            run.result.not_captured,
            o.secrets_redacted,
            o.tokens_o200k,
            tm_core::tokens::claude_estimate(o.tokens_o200k),
            o.outputs.len()
        );
        for p in &o.outputs {
            text.push_str(&format!("output: {}\n", p.display()));
        }
        Ok(text)
    }

    fn tool_repo_map(&self, args: &Value) -> Result<String, String> {
        let src = arg_str(args, "path").ok_or("path is required")?;
        let tokens = arg_usize(args, "tokens").unwrap_or(1024);
        let source = self.resolve_source(src)?;
        let mut options = MergeOptions::for_folder(source.root.clone());
        options.include_tree = false;
        let job = tm_core::job::resolve_job(&options)?;
        let scan = tm_core::scanner::scan_text_files(&job.root, &job.scan_options)
            .map_err(|e| format!("scan failed: {}", e))?;
        Ok(tm_core::repomap::build_repo_map(
            &job.root,
            &scan.files,
            tokens,
        ))
    }

    fn tool_read_output(&self, args: &Value) -> Result<String, String> {
        let path = arg_str(args, "path").ok_or("path is required")?;
        let p = self.checked_output_path(path)?;
        let p = p.as_path();
        let offset = arg_usize(args, "offset").unwrap_or(0);
        let limit = arg_usize(args, "limit")
            .unwrap_or(READ_DEFAULT_LINES)
            .clamp(1, READ_MAX_LINES);
        let content = std::fs::read_to_string(p).map_err(|e| format!("read failed: {}", e))?;
        let total = content.lines().count();
        let mut out = format!(
            "{} — lines {}..{} of {}\n",
            p.display(),
            offset + 1,
            (offset + limit).min(total),
            total
        );
        let mut bytes = 0usize;
        for line in content.lines().skip(offset).take(limit) {
            bytes += line.len() + 1;
            if bytes > READ_MAX_BYTES {
                out.push_str("[... slice truncated at 200 KB — narrow the range ...]\n");
                break;
            }
            out.push_str(line);
            out.push('\n');
        }
        Ok(out)
    }

    fn tool_grep_output(&self, args: &Value) -> Result<String, String> {
        let path = arg_str(args, "path").ok_or("path is required")?;
        let pattern = arg_str(args, "pattern").ok_or("pattern is required")?;
        let p = self.checked_output_path(path)?;
        let p = p.as_path();
        let max = arg_usize(args, "max_matches")
            .unwrap_or(GREP_MAX_MATCHES)
            .clamp(1, GREP_MAX_MATCHES);
        let re = regex::Regex::new(pattern).map_err(|e| format!("bad regex: {}", e))?;
        let content = std::fs::read_to_string(p).map_err(|e| format!("read failed: {}", e))?;
        let mut out = String::new();
        let mut hits = 0usize;
        for (i, line) in content.lines().enumerate() {
            if re.is_match(line) {
                hits += 1;
                let shown: String = line.chars().take(400).collect();
                out.push_str(&format!("{}: {}\n", i + 1, shown));
                if hits >= max {
                    out.push_str(&format!("[... stopped at {} matches ...]\n", max));
                    break;
                }
            }
        }
        if hits == 0 {
            out.push_str("no matches\n");
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
