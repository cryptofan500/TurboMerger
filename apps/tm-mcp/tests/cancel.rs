//! `notifications/cancelled` stops a running pack_directory (plan §10.6):
//! no response, no output, and the server keeps serving. Its own test
//! binary because it slows file processing through a process-wide env var.

use std::time::{Duration, Instant};

use rmcp::ServiceExt;
use serde_json::{json, Value};
use tm_mcp::{McpConfig, McpServer};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn cancelled_pack_writes_nothing_and_the_server_lives_on() {
    // Debug-build hook in tm-core: every file takes 300 ms.
    std::env::set_var("TM_TEST_SLOW_FILE_MS", "300");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("shared").join("slow");
    std::fs::create_dir_all(&root).unwrap();
    for i in 0..200 {
        std::fs::write(
            root.join(format!("f{i:03}.rs")),
            format!("fn f{i}() {{}}\n"),
        )
        .unwrap();
    }
    let outs = tmp.path().join("outs");
    let server = McpServer::from_config(&McpConfig {
        roots: vec![tmp.path().join("shared")],
        output_dir: Some(outs.clone()),
        allow_remote: false,
    })
    .unwrap();

    let (client, server_io) = tokio::io::duplex(1 << 20);
    tokio::spawn(async move {
        if let Ok(svc) = server.serve(server_io).await {
            let _ = svc.waiting().await;
        }
    });
    let (r, mut w) = tokio::io::split(client);
    let mut lines = BufReader::new(r).lines();
    async fn send(w: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>, v: Value) {
        w.write_all(format!("{}\n", v).as_bytes()).await.unwrap();
    }

    send(
        &mut w,
        json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}}}),
    )
    .await;
    let init = lines.next_line().await.unwrap().unwrap();
    assert!(init.contains("\"id\":0"), "{init}");
    send(
        &mut w,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;

    send(
        &mut w,
        json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {
            "name": "pack_directory", "arguments": {"path": root.to_string_lossy()}}}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let asked = Instant::now();
    send(
        &mut w,
        json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
               "params": {"requestId": 7, "reason": "test"}}),
    )
    .await;

    // The server answers a ping; whatever it says about request 7 must not
    // be a successful pack.
    send(&mut w, json!({"jsonrpc": "2.0", "id": 8, "method": "ping"})).await;
    loop {
        let line = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
            .await
            .expect("server responsive")
            .unwrap()
            .unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        if v["id"] == 7 {
            assert_ne!(
                v["result"]["isError"], false,
                "a cancelled pack must not succeed: {v}"
            );
        }
        if v["id"] == 8 {
            break;
        }
    }
    // The job stops within a second of the cancel (the chunk in flight
    // finishes its 300 ms files), and nothing is written.
    let deadline = asked + Duration::from_secs(3);
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let written: Vec<_> = std::fs::read_dir(&outs)
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert!(written.is_empty(), "cancelled pack wrote {written:?}");
}
