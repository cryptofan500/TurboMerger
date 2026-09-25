//! MCP server sidecar (T3-2): `turbomerger mcp` speaks Model Context
//! Protocol over stdio (newline-delimited JSON-RPC 2.0) so Claude
//! Desktop/Code and other MCP clients can pull repo context on demand.
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
//! Protocol versions (N-19): the server answers with the client's version
//! when it supports it, otherwise with its latest — v7.7.0 echoed anything,
//! including `2099-01-01`.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use serde_json::{json, Value};

use crate::cli::McpCmd;
use crate::commands::{resolve_job, MergeOptions};

/// Newest first; the server's answer when the client asks for anything else.
const SUPPORTED_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const READ_DEFAULT_LINES: usize = 200;
const READ_MAX_LINES: usize = 1000;
const READ_MAX_BYTES: usize = 200 * 1024;
const GREP_MAX_MATCHES: usize = 100;

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
    pub fn from_args(args: &McpCmd) -> Result<McpServer, String> {
        let mut roots = Vec::new();
        let requested: Vec<PathBuf> = if args.root.is_empty() {
            let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
            let home = dirs::home_dir();
            let too_broad = cwd.parent().is_none() || home.as_deref() == Some(cwd.as_path());
            if too_broad {
                Vec::new()
            } else {
                vec![cwd]
            }
        } else {
            args.root.clone()
        };
        for r in requested {
            let c = crate::security::validate_and_canonicalize(&r.to_string_lossy())
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
        let output_dir = std::fs::canonicalize(&output_dir).map_err(|e| e.to_string())?;
        Ok(McpServer {
            roots,
            output_dir,
            allow_remote: args.allow_remote,
        })
    }

    /// Resolve a client-supplied source: an explicit remote reference (when
    /// allowed) or a folder under one of the roots.
    fn resolve_source(
        &self,
        src: &str,
    ) -> Result<
        (
            String,
            Option<crate::remote::RemoteCheckout>,
            Option<String>,
        ),
        String,
    > {
        if crate::remote::parse_remote_explicit(src).is_some() && !Path::new(src).exists() {
            if !self.allow_remote {
                return Err(
                    "remote repositories are disabled for this server (start it with --allow-remote)"
                        .into(),
                );
            }
            return crate::cli::resolve_source(src);
        }
        if self.roots.is_empty() {
            return Err(
                "no folder is shared with MCP clients — start the server with `turbomerger mcp --root <DIR>`"
                    .into(),
            );
        }
        let canon = crate::security::validate_and_canonicalize(src)
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
        Ok((canon.to_string_lossy().to_string(), None, None))
    }

    /// A path inside the managed outputs directory, or an error.
    fn checked_output_path(&self, path: &str) -> Result<PathBuf, String> {
        let p = Path::new(path);
        let candidate = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.output_dir.join(p)
        };
        let canon = std::fs::canonicalize(&candidate)
            .map_err(|_| format!("not a TurboMerger output: {}", path))?;
        if !canon.starts_with(&self.output_dir) || !canon.is_file() {
            return Err(format!(
                "read_output/grep_output only serve files in {}",
                self.output_dir.display()
            ));
        }
        Ok(canon)
    }
}

/// Blocking stdio loop. Returns the process exit code.
pub fn run_mcp(args: McpCmd) -> i32 {
    let server = match McpServer::from_args(&args) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };
    if server.roots.is_empty() {
        eprintln!("warning: no --root given and the working directory is / or your home folder; pack_directory and repo_map will refuse local paths");
    }
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(resp) = server.handle_message(&line) {
            if out.write_all(resp.as_bytes()).is_err()
                || out.write_all(b"\n").is_err()
                || out.flush().is_err()
            {
                break;
            }
        }
    }
    0
}

impl McpServer {
    /// Handle one JSON-RPC message; `None` = nothing to send (notification).
    /// Pure function of the message so the protocol is unit-testable.
    pub fn handle_message(&self, line: &str) -> Option<String> {
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                return Some(
                    json!({"jsonrpc":"2.0","id":null,
                           "error":{"code":-32700,"message":format!("parse error: {}", e)}})
                    .to_string(),
                )
            }
        };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        // Notifications (no id) get no response, per JSON-RPC.
        let id = id?;

        let result: Result<Value, (i64, String)> = match method {
            "initialize" => {
                let requested = msg
                    .pointer("/params/protocolVersion")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let version = SUPPORTED_VERSIONS
                    .iter()
                    .find(|v| **v == requested)
                    .copied()
                    .unwrap_or(SUPPORTED_VERSIONS[0]);
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": {
                        "name": "turbomerger",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tool_definitions() })),
            "tools/call" => {
                let name = msg
                    .pointer("/params/name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let default_args = json!({});
                let args = msg.pointer("/params/arguments").unwrap_or(&default_args);
                let outcome = match name {
                    "pack_directory" => self.tool_pack_directory(args),
                    "repo_map" => self.tool_repo_map(args),
                    "read_output" => self.tool_read_output(args),
                    "grep_output" => self.tool_grep_output(args),
                    other => Err(format!("unknown tool: {}", other)),
                };
                // Tool-level failures are results with isError, not protocol errors.
                Ok(match outcome {
                    Ok(text) => json!({"content":[{"type":"text","text":text}],"isError":false}),
                    Err(e) => json!({"content":[{"type":"text","text":e}],"isError":true}),
                })
            }
            other => Err((-32601, format!("method not found: {}", other))),
        };

        Some(
            match result {
                Ok(res) => json!({"jsonrpc":"2.0","id":id,"result":res}),
                Err((code, message)) => {
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
                }
            }
            .to_string(),
        )
    }
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "pack_directory",
            "description": "Merge a folder under the server's shared roots (or, with --allow-remote, a repo URL / gh:owner/repo) into one LLM-ready file (gitignore-aware, secrets always redacted). Returns a summary plus the output path(s); use read_output/grep_output to access the content.",
            "inputSchema": {
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
            }
        },
        {
            "name": "repo_map",
            "description": "Aider-style repo map: ranked file signatures (tree-sitter tags + PageRank) rendered to a token budget. The best first look at a repo that won't fit in context.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Folder under the server's shared roots, or (with --allow-remote) a repo URL / gh:owner/repo" },
                    "tokens": { "type": "integer", "description": "Token budget (default 1024)" }
                },
                "required": ["path"]
            }
        },
        {
            "name": "read_output",
            "description": "Read a slice of a TurboMerger output (a path returned by pack_directory) by line offset/limit.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": "integer", "description": "0-based first line (default 0)" },
                    "limit": { "type": "integer", "description": "Lines to return (default 200, max 1000)" }
                },
                "required": ["path"]
            }
        },
        {
            "name": "grep_output",
            "description": "Regex-search a TurboMerger output (a path returned by pack_directory); returns matching lines with line numbers (max 100).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "pattern": { "type": "string", "description": "Rust regex" },
                    "max_matches": { "type": "integer" }
                },
                "required": ["path", "pattern"]
            }
        }
    ])
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
    fn tool_pack_directory(&self, args: &Value) -> Result<String, String> {
        let src = arg_str(args, "path").ok_or("path is required")?;
        let (root, _checkout, remote_label) = self.resolve_source(src)?;
        let format = arg_str(args, "format").map(|s| s.to_string());
        // The client names the file at most; the directory is always ours.
        let output_path = arg_str(args, "output").map(|hint| {
            let base = Path::new(hint)
                .file_name()
                .map(|n| crate::security::sanitize_filename(&n.to_string_lossy()))
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| "merged".into());
            self.output_dir.join(base).to_string_lossy().to_string()
        });
        let options = MergeOptions {
            folder_path: root,
            output_path: Some(
                output_path.unwrap_or_else(|| self.output_dir.to_string_lossy().to_string()),
            ),
            include_venv: false,
            include_tree: true,
            content_detection: true,
            respect_gitignore: !arg_bool(args, "no_gitignore"),
            include_hidden: arg_bool(args, "include_hidden"),
            // Forced: an MCP client must never produce an unredacted dump.
            redact_secrets: true,
            format,
            ordering: arg_str(args, "ordering").map(|s| s.to_string()),
            max_tokens: arg_usize(args, "max_tokens"),
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
            remove_empty_lines: false,
            truncate_base64: false,
            compress: arg_bool(args, "compress"),
            strip_comments: arg_bool(args, "strip_comments"),
            git_diff: arg_bool(args, "git_diff"),
            git_log_count: arg_usize(args, "git_log").unwrap_or(0),
            emit_skill: arg_bool(args, "emit_skill"),
            selected_paths: None,
            force_include: Vec::new(),
            show_source_path: false,
            remote: remote_label.is_some(),
            source_label: remote_label,
            config_path: None,
            max_file_size_mb: None,
        };
        let job = resolve_job(&options)?;
        if !job.output_path.starts_with(&self.output_dir) {
            return Err("output must stay in the outputs folder".into());
        }
        let scan = crate::scanner::scan_text_files(&job.root, &job.scan_options)
            .map_err(|e| format!("scan failed: {}", e))?;
        let cancel = AtomicBool::new(false);
        let outcome = crate::merger::merge_files_with_progress(
            &job.root,
            &scan.files,
            &job.output_path,
            &job.merge_config,
            &cancel,
            |_, _, _| {},
            &scan.skipped,
        )
        .map_err(|e| format!("merge failed: {}", e))?;
        let not_captured = scan
            .skipped
            .iter()
            .chain(outcome.skipped.iter())
            .filter(|s| s.kind == crate::scanner::SkipKind::NotCaptured)
            .count();
        let mut text = format!(
        "merged={} skipped={} not_captured={} secrets_redacted={} tokens_o200k={} (~{} Claude est.) parts={}\n",
        outcome.files_processed,
        outcome.files_skipped + scan.skipped.len(),
        not_captured,
        outcome.secrets_redacted,
        outcome.tokens_o200k,
        crate::tokens::claude_estimate(outcome.tokens_o200k),
        outcome.outputs.len()
    );
        for p in &outcome.outputs {
            text.push_str(&format!("output: {}\n", p.display()));
        }
        Ok(text)
    }

    fn tool_repo_map(&self, args: &Value) -> Result<String, String> {
        let src = arg_str(args, "path").ok_or("path is required")?;
        let tokens = arg_usize(args, "tokens").unwrap_or(1024);
        let (root, _checkout, _) = self.resolve_source(src)?;
        let options = MergeOptions {
            folder_path: root,
            output_path: None,
            include_venv: false,
            include_tree: false,
            content_detection: true,
            respect_gitignore: true,
            include_hidden: false,
            redact_secrets: true,
            format: None,
            ordering: None,
            max_tokens: None,
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
            remove_empty_lines: false,
            truncate_base64: false,
            compress: false,
            strip_comments: false,
            git_diff: false,
            git_log_count: 0,
            emit_skill: false,
            selected_paths: None,
            force_include: Vec::new(),
            show_source_path: false,
            source_label: None,
            remote: false,
            config_path: None,
            max_file_size_mb: None,
        };
        let job = resolve_job(&options)?;
        let scan = crate::scanner::scan_text_files(&job.root, &job.scan_options)
            .map_err(|e| format!("scan failed: {}", e))?;
        Ok(crate::repomap::build_repo_map(
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
mod tests {
    use super::*;

    struct Env {
        server: McpServer,
        tmp: tempfile::TempDir,
    }

    fn env() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let shared = tmp.path().join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        let args = McpCmd {
            root: vec![shared],
            output_dir: Some(tmp.path().join("outs")),
            allow_remote: false,
        };
        Env {
            server: McpServer::from_args(&args).unwrap(),
            tmp,
        }
    }

    fn call(server: &McpServer, line: &str) -> Value {
        serde_json::from_str(&server.handle_message(line).expect("response")).unwrap()
    }

    #[test]
    fn initialize_negotiates_a_supported_version() {
        let e = env();
        let resp = call(
            &e.server,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
        );
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(resp["result"]["serverInfo"]["name"], "turbomerger");
        assert!(resp["result"]["capabilities"]["tools"].is_object());
        // v1 A.9: an unknown version is answered with ours, never echoed.
        let resp = call(
            &e.server,
            r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2099-01-01"}}"#,
        );
        assert_eq!(resp["result"]["protocolVersion"], SUPPORTED_VERSIONS[0]);

        // notifications get no response
        assert!(e
            .server
            .handle_message(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .is_none());
        let pong = call(&e.server, r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#);
        assert!(pong["result"].is_object());
    }

    #[test]
    fn tools_list_names_all_four() {
        let e = env();
        let resp = call(
            &e.server,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#,
        );
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["pack_directory", "repo_map", "read_output", "grep_output"]
        );
    }

    #[test]
    fn unknown_method_and_unknown_tool() {
        let e = env();
        let resp = call(
            &e.server,
            r#"{"jsonrpc":"2.0","id":4,"method":"bogus/thing"}"#,
        );
        assert_eq!(resp["error"]["code"], -32601);
        let resp = call(
            &e.server,
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"nope","arguments":{}}}"#,
        );
        assert_eq!(resp["result"]["isError"], true);
    }

    fn tool(server: &McpServer, name: &str, args: Value) -> (bool, String) {
        let req = json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":name,"arguments":args}});
        let resp = call(server, &req.to_string());
        (
            resp["result"]["isError"] == false,
            resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or("")
                .to_string(),
        )
    }

    #[test]
    fn pack_repo_map_read_grep_roundtrip() {
        let e = env();
        let root = e.tmp.path().join("shared").join("mcp_repo");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/alpha.rs"),
            "pub fn alpha_one(v: u32) -> u32 {\n    v + 1\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/beta.rs"),
            "pub fn beta_two() -> u32 {\n    crate::alpha::alpha_one(1)\n}\n",
        )
        .unwrap();

        let (ok, text) = tool(
            &e.server,
            "pack_directory",
            json!({"path": root.to_string_lossy()}),
        );
        assert!(ok, "{text}");
        assert!(text.contains("merged=2"), "summary: {}", text);
        let out_path = text
            .lines()
            .find_map(|l| l.strip_prefix("output: "))
            .expect("output path in summary")
            .to_string();
        assert!(
            Path::new(&out_path).starts_with(&e.server.output_dir),
            "{out_path}"
        );

        let (_, map) = tool(
            &e.server,
            "repo_map",
            json!({"path": root.to_string_lossy(), "tokens": 500}),
        );
        assert!(map.contains("alpha_one"), "map: {}", map);

        let (_, slice) = tool(
            &e.server,
            "read_output",
            json!({"path": out_path, "offset": 0, "limit": 5}),
        );
        assert!(slice.contains("lines 1..5"), "slice: {}", slice);

        let (_, hits) = tool(
            &e.server,
            "grep_output",
            json!({"path": out_path, "pattern": "alpha_one"}),
        );
        assert!(hits.contains("alpha_one"), "grep: {}", hits);

        // Arbitrary files are refused, even ones named like outputs.
        let secret = e.tmp.path().join("x_merged.md");
        std::fs::write(&secret, "nope\n").unwrap();
        let (ok, _) = tool(
            &e.server,
            "read_output",
            json!({"path": secret.to_string_lossy()}),
        );
        assert!(!ok);
    }

    #[test]
    fn a9_outputs_never_leave_the_outputs_folder() {
        // v1 A.9: output = ".../.bashrc_probe" was written verbatim.
        let e = env();
        let root = e.tmp.path().join("shared").join("r");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        let probe = e.tmp.path().join("mcp_any").join(".bashrc_probe");
        let (ok, text) = tool(
            &e.server,
            "pack_directory",
            json!({"path": root.to_string_lossy(), "output": probe.to_string_lossy()}),
        );
        assert!(ok, "{text}");
        assert!(!probe.exists(), "must not write where the client says");
        let out = text
            .lines()
            .find_map(|l| l.strip_prefix("output: "))
            .unwrap();
        assert!(Path::new(out).starts_with(&e.server.output_dir));
        assert!(out.ends_with("bashrc_probe"), "{out}");
    }

    #[test]
    fn sources_outside_the_shared_roots_are_refused() {
        let e = env();
        let outside = e.tmp.path().join("private");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("k.rs"), "fn k() {}\n").unwrap();
        let (ok, text) = tool(
            &e.server,
            "pack_directory",
            json!({"path": outside.to_string_lossy()}),
        );
        assert!(!ok && text.contains("outside"), "{text}");
        let (ok, _) = tool(
            &e.server,
            "repo_map",
            json!({"path": outside.to_string_lossy()}),
        );
        assert!(!ok);
        // Remote sources need --allow-remote.
        let (ok, text) = tool(
            &e.server,
            "pack_directory",
            json!({"path": "gh:rust-lang/cargo"}),
        );
        assert!(!ok && text.contains("--allow-remote"), "{text}");
    }
}
