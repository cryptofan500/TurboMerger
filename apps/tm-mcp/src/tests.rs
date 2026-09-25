//! Protocol-level tests: a raw JSON-RPC client talks to the rmcp service
//! over an in-memory duplex stream, exactly as a client would over stdio.

use super::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub(crate) struct Session {
    write: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    lines: tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    pub(crate) notifications: Vec<Value>,
    _server: tokio::task::JoinHandle<()>,
}

impl Session {
    pub(crate) async fn start(server: McpServer) -> Session {
        let (client, server_io) = tokio::io::duplex(1 << 20);
        let handle = tokio::spawn(async move {
            if let Ok(svc) = server.serve(server_io).await {
                let _ = svc.waiting().await;
            }
        });
        let (r, w) = tokio::io::split(client);
        Session {
            write: w,
            lines: BufReader::new(r).lines(),
            notifications: Vec::new(),
            _server: handle,
        }
    }

    pub(crate) async fn send(&mut self, v: Value) {
        let mut s = v.to_string();
        s.push('\n');
        self.write.write_all(s.as_bytes()).await.unwrap();
    }

    pub(crate) async fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        self.response(id).await
    }

    /// The response with `id`; notifications on the way are kept.
    pub(crate) async fn response(&mut self, id: u64) -> Value {
        loop {
            let line = tokio::time::timeout(Duration::from_secs(120), self.lines.next_line())
                .await
                .expect("a timely response")
                .expect("readable")
                .expect("server still connected");
            let v: Value = serde_json::from_str(&line).unwrap();
            if v.get("id") == Some(&json!(id)) {
                return v;
            }
            self.notifications.push(v);
        }
    }

    pub(crate) async fn initialize(&mut self, version: &str) -> Value {
        let r = self
            .request(
                0,
                "initialize",
                json!({
                    "protocolVersion": version,
                    "capabilities": {},
                    "clientInfo": {"name": "test", "version": "0"}
                }),
            )
            .await;
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        r
    }

    pub(crate) async fn tool(&mut self, id: u64, name: &str, args: Value) -> (bool, String) {
        let resp = self
            .request(id, "tools/call", json!({"name": name, "arguments": args}))
            .await;
        (
            resp["result"]["isError"] == false,
            resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or("")
                .to_string(),
        )
    }
}

pub(crate) struct Env {
    pub(crate) server: McpServer,
    pub(crate) tmp: tempfile::TempDir,
}

pub(crate) fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let shared = tmp.path().join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    let args = McpConfig {
        roots: vec![shared],
        output_dir: Some(tmp.path().join("outs")),
        allow_remote: false,
    };
    Env {
        server: McpServer::from_config(&args).unwrap(),
        tmp,
    }
}

#[tokio::test]
async fn initialize_negotiates_a_supported_version() {
    let e = env();
    let mut s = Session::start(e.server.clone()).await;
    let resp = s.initialize("2025-03-26").await;
    assert_eq!(resp["result"]["protocolVersion"], "2025-03-26", "{resp}");
    assert_eq!(resp["result"]["serverInfo"]["name"], "turbomerger");
    assert!(resp["result"]["capabilities"]["tools"].is_object());
    let pong = s.request(1, "ping", json!({})).await;
    assert!(pong["result"].is_object(), "{pong}");

    // v1 A.9: an unknown version is answered with ours, never echoed.
    let mut s = Session::start(e.server.clone()).await;
    let resp = s.initialize("2099-01-01").await;
    let v = resp["result"]["protocolVersion"].as_str().unwrap();
    assert_ne!(v, "2099-01-01");
    assert_eq!(v, "2025-11-25", "{resp}");
}

#[tokio::test]
async fn tools_list_names_all_four() {
    let e = env();
    let mut s = Session::start(e.server.clone()).await;
    s.initialize("2025-11-25").await;
    let resp = s.request(3, "tools/list", json!({})).await;
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

#[tokio::test]
async fn unknown_method_and_unknown_tool() {
    let e = env();
    let mut s = Session::start(e.server.clone()).await;
    s.initialize("2025-11-25").await;
    let resp = s.request(4, "bogus/thing", json!({})).await;
    assert_eq!(resp["error"]["code"], -32601, "{resp}");
    let (ok, text) = s.tool(5, "nope", json!({})).await;
    assert!(!ok && text.contains("unknown tool"), "{text}");
}

#[tokio::test]
async fn pack_repo_map_read_grep_roundtrip() {
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
    let mut s = Session::start(e.server.clone()).await;
    s.initialize("2025-11-25").await;

    let (ok, text) = s
        .tool(
            10,
            "pack_directory",
            json!({"path": root.to_string_lossy()}),
        )
        .await;
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

    let (_, map) = s
        .tool(
            11,
            "repo_map",
            json!({"path": root.to_string_lossy(), "tokens": 500}),
        )
        .await;
    assert!(map.contains("alpha_one"), "map: {}", map);

    let (_, slice) = s
        .tool(
            12,
            "read_output",
            json!({"path": out_path, "offset": 0, "limit": 5}),
        )
        .await;
    assert!(slice.contains("lines 1..5"), "slice: {}", slice);

    let (_, hits) = s
        .tool(
            13,
            "grep_output",
            json!({"path": out_path, "pattern": "alpha_one"}),
        )
        .await;
    assert!(hits.contains("alpha_one"), "grep: {}", hits);

    // Arbitrary files are refused, even ones named like outputs.
    let secret = e.tmp.path().join("x_merged.md");
    std::fs::write(&secret, "nope\n").unwrap();
    let (ok, _) = s
        .tool(14, "read_output", json!({"path": secret.to_string_lossy()}))
        .await;
    assert!(!ok);
}

#[tokio::test]
async fn a9_outputs_never_leave_the_outputs_folder() {
    // v1 A.9: output = ".../.bashrc_probe" was written verbatim.
    let e = env();
    let root = e.tmp.path().join("shared").join("r");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
    let probe = e.tmp.path().join("mcp_any").join(".bashrc_probe");
    let mut s = Session::start(e.server.clone()).await;
    s.initialize("2025-11-25").await;
    let (ok, text) = s
        .tool(
            20,
            "pack_directory",
            json!({"path": root.to_string_lossy(), "output": probe.to_string_lossy()}),
        )
        .await;
    assert!(ok, "{text}");
    assert!(!probe.exists(), "must not write where the client says");
    let out = text
        .lines()
        .find_map(|l| l.strip_prefix("output: "))
        .unwrap();
    assert!(Path::new(out).starts_with(&e.server.output_dir));
    assert!(out.ends_with("bashrc_probe"), "{out}");
}

#[tokio::test]
async fn sources_outside_the_shared_roots_are_refused() {
    let e = env();
    let outside = e.tmp.path().join("private");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("k.rs"), "fn k() {}\n").unwrap();
    let mut s = Session::start(e.server.clone()).await;
    s.initialize("2025-11-25").await;
    let (ok, text) = s
        .tool(
            30,
            "pack_directory",
            json!({"path": outside.to_string_lossy()}),
        )
        .await;
    assert!(!ok && text.contains("outside"), "{text}");
    let (ok, _) = s
        .tool(31, "repo_map", json!({"path": outside.to_string_lossy()}))
        .await;
    assert!(!ok);
    // Remote sources need --allow-remote.
    let (ok, text) = s
        .tool(32, "pack_directory", json!({"path": "gh:rust-lang/cargo"}))
        .await;
    assert!(!ok && text.contains("--allow-remote"), "{text}");
}

#[tokio::test]
async fn pack_reports_progress_to_clients_that_ask() {
    let e = env();
    let root = e.tmp.path().join("shared").join("many");
    std::fs::create_dir_all(&root).unwrap();
    for i in 0..300 {
        std::fs::write(
            root.join(format!("f{i:03}.rs")),
            format!("fn f{i}() {{}}\n"),
        )
        .unwrap();
    }
    let mut s = Session::start(e.server.clone()).await;
    s.initialize("2025-11-25").await;
    let resp = s
        .request(
            40,
            "tools/call",
            json!({
                "name": "pack_directory",
                "arguments": {"path": root.to_string_lossy()},
                "_meta": {"progressToken": "p-40"}
            }),
        )
        .await;
    assert_eq!(resp["result"]["isError"], false, "{resp}");
    let progress: Vec<&Value> = s
        .notifications
        .iter()
        .filter(|n| n["method"] == "notifications/progress")
        .collect();
    assert!(!progress.is_empty(), "{:?}", s.notifications);
    assert!(progress
        .iter()
        .all(|n| n["params"]["progressToken"] == "p-40"));
}
