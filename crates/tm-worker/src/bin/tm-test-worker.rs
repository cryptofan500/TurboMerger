//! The worker host's test double. Job kinds:
//! - `echo`: returns its input;
//! - `progress`: reports `items` items, `ms` apart, then returns;
//! - `hang`: reports one item, then never answers again;
//! - `oom`: allocates (and touches) memory until the cap kills it;
//! - `crash`: aborts; `fail`: reports an error; `garbage`: prints non-JSON.

use std::io::BufRead;
use std::time::Duration;

use serde_json::json;
use tm_worker::{send, ToHost, ToWorker, PROTOCOL};

fn progress(id: &str, item: String, done: u64, total: u64) {
    send(&ToHost::Progress {
        id: id.to_string(),
        item,
        done,
        total,
        message: String::new(),
    });
}

fn main() {
    send(&ToHost::Hello {
        // `--protocol N` pretends to be another protocol version.
        protocol: std::env::args()
            .skip_while(|a| a != "--protocol")
            .nth(1)
            .and_then(|v| v.parse().ok())
            .unwrap_or(PROTOCOL),
        worker: "tm-test-worker".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: [
            "echo", "progress", "hang", "oom", "crash", "fail", "garbage",
        ]
        .map(String::from)
        .to_vec(),
    });
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<ToWorker>(&line) else {
            eprintln!("bad request: {line}");
            continue;
        };
        let (id, kind, input) = match msg {
            ToWorker::Shutdown => break,
            ToWorker::Job { id, kind, input } => (id, kind, input),
        };
        match kind.as_str() {
            "echo" => send(&ToHost::Result { id, output: input }),
            "progress" => {
                let n = input["items"].as_u64().unwrap_or(3);
                let ms = input["ms"].as_u64().unwrap_or(10);
                for i in 0..n {
                    std::thread::sleep(Duration::from_millis(ms));
                    progress(&id, format!("item {}", i + 1), i + 1, n);
                }
                send(&ToHost::Result {
                    id,
                    output: json!({ "items": n }),
                });
            }
            "hang" => {
                let item = input["item"].as_str().unwrap_or("stuck item").to_string();
                progress(&id, item, 0, 1);
                loop {
                    std::thread::sleep(Duration::from_secs(3600));
                }
            }
            "oom" => {
                eprintln!("allocating until the cap stops me");
                let mut hog: Vec<Vec<u8>> = Vec::new();
                loop {
                    hog.push(vec![1u8; 64 << 20]);
                    progress(&id, format!("{} MiB", hog.len() * 64), hog.len() as u64, 0);
                }
            }
            "crash" => {
                eprintln!("crashing on purpose");
                std::process::abort();
            }
            "fail" => send(&ToHost::Error {
                id,
                message: "requested failure".into(),
            }),
            "garbage" => println!("this is not a protocol line"),
            other => send(&ToHost::Error {
                id,
                message: format!("unknown kind {}", other),
            }),
        }
    }
}
