//! MCP server sidecar (T3-2): `turbomerger mcp` speaks Model Context
//! Protocol over stdio (newline-delimited JSON-RPC 2.0) so Claude
//! Desktop/Code and other MCP clients can pull repo context on demand.
//!
//! Tools: pack_directory (full merge → file, summary returned),
//! repo_map (map text returned inline), read_output / grep_output
//! (sliced access to previously produced `*_merged.*` files — deliberately
//! NOT general-purpose file readers).
//!
//! Safety: secret redaction is forced ON for MCP-driven merges; read/grep
//! refuse paths that aren't TurboMerger outputs.

use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::atomic::AtomicBool;

use serde_json::{json, Value};

use crate::commands::{resolve_cli_source, resolve_job, MergeOptions};

const PROTOCOL_VERSION: &str = "2024-11-05";
const READ_DEFAULT_LINES: usize = 200;
const READ_MAX_LINES: usize = 1000;
const READ_MAX_BYTES: usize = 200 * 1024;
const GREP_MAX_MATCHES: usize = 100;

/// Blocking stdio loop. Returns the process exit code.
pub fn run_mcp() -> i32 {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(resp) = handle_message(&line) {
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

/// Handle one JSON-RPC message; `None` = nothing to send (notification).
/// Pure function so the whole protocol is unit-testable.
pub fn handle_message(line: &str) -> Option<String> {
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
            let client_version = msg
                .pointer("/params/protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or(PROTOCOL_VERSION);
            Ok(json!({
                "protocolVersion": client_version,
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
                "pack_directory" => tool_pack_directory(args),
                "repo_map" => tool_repo_map(args),
                "read_output" => tool_read_output(args),
                "grep_output" => tool_grep_output(args),
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

fn tool_definitions() -> Value {
    json!([
        {
            "name": "pack_directory",
            "description": "Merge a local directory or remote repo (owner/repo or URL, shallow-cloned) into one LLM-ready file (gitignore-aware, secrets always redacted). Returns a summary plus the output path(s); use read_output/grep_output to access the content.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Local directory, owner/repo, or repo URL" },
                    "output": { "type": "string", "description": "Output file or directory (default: Downloads)" },
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
                    "path": { "type": "string", "description": "Local directory, owner/repo, or repo URL" },
                    "tokens": { "type": "integer", "description": "Token budget (default 1024)" }
                },
                "required": ["path"]
            }
        },
        {
            "name": "read_output",
            "description": "Read a slice of a TurboMerger output file (*_merged.*) by line offset/limit.",
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
            "description": "Regex-search a TurboMerger output file (*_merged.*); returns matching lines with line numbers (max 100).",
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
    args.get(key).and_then(|v| v.as_str()).filter(|s| !s.is_empty())
}
fn arg_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}
fn arg_usize(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(|v| v.as_u64()).map(|n| n as usize)
}

fn tool_pack_directory(args: &Value) -> Result<String, String> {
    let src = arg_str(args, "path").ok_or("path is required")?;
    let (root, _checkout) = resolve_cli_source(src)?;
    let options = MergeOptions {
        folder_path: root,
        output_path: arg_str(args, "output").map(|s| s.to_string()),
        include_venv: false,
        include_tree: true,
        content_detection: true,
        respect_gitignore: !arg_bool(args, "no_gitignore"),
        include_hidden: arg_bool(args, "include_hidden"),
        // Forced: an MCP client must never produce an unredacted dump.
        redact_secrets: true,
        format: arg_str(args, "format").map(|s| s.to_string()),
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
    };
    let job = resolve_job(&options)?;
    let scan = crate::scanner::scan_text_files(&job.root, &job.scan_options)
        .map_err(|e| format!("scan failed: {}", e))?;
    if scan.files.is_empty() {
        return Err("no text files found".to_string());
    }
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
    let mut text = format!(
        "merged={} skipped={} secrets_redacted={} tokens_o200k={} (~{} Claude est.) parts={}\n",
        outcome.files_processed,
        outcome.files_skipped + scan.skipped.len(),
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

fn tool_repo_map(args: &Value) -> Result<String, String> {
    let src = arg_str(args, "path").ok_or("path is required")?;
    let tokens = arg_usize(args, "tokens").unwrap_or(1024);
    let (root, _checkout) = resolve_cli_source(src)?;
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
    };
    let job = resolve_job(&options)?;
    let scan = crate::scanner::scan_text_files(&job.root, &job.scan_options)
        .map_err(|e| format!("scan failed: {}", e))?;
    Ok(crate::repomap::build_repo_map(&job.root, &scan.files, tokens))
}

/// Guard: read/grep serve TurboMerger outputs only, not arbitrary files.
fn checked_output_path(path: &str) -> Result<&Path, String> {
    let p = Path::new(path);
    let name = p
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.to_ascii_lowercase())
        .unwrap_or_default();
    if !name.contains("_merged.") {
        return Err("read_output/grep_output only access TurboMerger outputs (*_merged.*)".into());
    }
    if !p.is_file() {
        return Err(format!("not a file: {}", path));
    }
    Ok(p)
}

fn tool_read_output(args: &Value) -> Result<String, String> {
    let path = arg_str(args, "path").ok_or("path is required")?;
    let p = checked_output_path(path)?;
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

fn tool_grep_output(args: &Value) -> Result<String, String> {
    let path = arg_str(args, "path").ok_or("path is required")?;
    let pattern = arg_str(args, "pattern").ok_or("pattern is required")?;
    let p = checked_output_path(path)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn call(line: &str) -> Value {
        serde_json::from_str(&handle_message(line).expect("response")).unwrap()
    }

    #[test]
    fn initialize_handshake_and_ping() {
        let resp = call(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
        );
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(resp["result"]["serverInfo"]["name"], "turbomerger");
        assert!(resp["result"]["capabilities"]["tools"].is_object());

        // notifications get no response
        assert!(handle_message(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .is_none());

        let pong = call(r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#);
        assert!(pong["result"].is_object());
    }

    #[test]
    fn tools_list_names_all_four() {
        let resp = call(r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#);
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
        let resp = call(r#"{"jsonrpc":"2.0","id":4,"method":"bogus/thing"}"#);
        assert_eq!(resp["error"]["code"], -32601);

        let resp = call(
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"nope","arguments":{}}}"#,
        );
        assert_eq!(resp["result"]["isError"], true);
    }

    #[test]
    fn pack_repo_map_read_grep_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("mcp_repo");
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
        let out_dir = tmp.path().join("outs");
        std::fs::create_dir_all(&out_dir).unwrap();

        // pack_directory
        let req = json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{
            "name":"pack_directory",
            "arguments":{"path": root.to_string_lossy(), "output": out_dir.to_string_lossy()}
        }});
        let resp = call(&req.to_string());
        assert_eq!(resp["result"]["isError"], false, "{:?}", resp);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("merged=2"), "summary: {}", text);
        let out_path = text
            .lines()
            .find_map(|l| l.strip_prefix("output: "))
            .expect("output path in summary")
            .to_string();
        assert!(Path::new(&out_path).is_file());

        // repo_map
        let req = json!({"jsonrpc":"2.0","id":11,"method":"tools/call","params":{
            "name":"repo_map","arguments":{"path": root.to_string_lossy(), "tokens": 500}
        }});
        let resp = call(&req.to_string());
        let map = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(map.contains("alpha_one"), "map: {}", map);

        // read_output slice
        let req = json!({"jsonrpc":"2.0","id":12,"method":"tools/call","params":{
            "name":"read_output","arguments":{"path": out_path, "offset": 0, "limit": 5}
        }});
        let resp = call(&req.to_string());
        let slice = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(slice.contains("lines 1..5"), "slice: {}", slice);

        // grep_output
        let req = json!({"jsonrpc":"2.0","id":13,"method":"tools/call","params":{
            "name":"grep_output","arguments":{"path": out_path, "pattern": "alpha_one"}
        }});
        let resp = call(&req.to_string());
        let hits = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(hits.contains("alpha_one"), "grep: {}", hits);

        // guard: arbitrary files are refused
        let secret = tmp.path().join("secrets.txt");
        std::fs::write(&secret, "nope\n").unwrap();
        let req = json!({"jsonrpc":"2.0","id":14,"method":"tools/call","params":{
            "name":"read_output","arguments":{"path": secret.to_string_lossy()}
        }});
        let resp = call(&req.to_string());
        assert_eq!(resp["result"]["isError"], true);
    }
}
