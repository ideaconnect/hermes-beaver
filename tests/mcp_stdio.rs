//! Drives the real binary: MCP over stdio, then `call`, against the same store.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_hermes-beaver");

fn temp_store(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("hb-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir.join("events.db")
}

#[test]
fn mcp_session_over_stdio() {
    let store = temp_store("mcp");
    let mut child = Command::new(BIN)
        .args(["--store", store.to_str().unwrap(), "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn hermes-beaver mcp");
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut ask = |stdin: &mut std::process::ChildStdin, msg: Value| -> Value {
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
        serde_json::from_str(&out.next().unwrap().unwrap()).unwrap()
    };

    let init = ask(
        &mut stdin,
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "it", "version": "1"}}}),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "hermes-beaver");

    // A notification gets no reply; the next answer must belong to id 2.
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
    )
    .unwrap();
    let list = ask(
        &mut stdin,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    );
    assert_eq!(list["id"], 2);
    assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 5);

    let started = ask(
        &mut stdin,
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {"name": "task_start", "arguments": {"source": "telegram", "title": "Summarise logs"}}}),
    );
    assert_eq!(started["result"]["isError"], false);
    let id = started["result"]["structuredContent"]["id"]
        .as_i64()
        .unwrap();

    drop(stdin); // EOF ends the server
    assert!(child.wait().unwrap().success());

    // `call` sees and updates the same store.
    let finish = Command::new(BIN)
        .args(["--store", store.to_str().unwrap(), "call", "task_finish"])
        .arg(json!({"id": id, "outcome": "done"}).to_string())
        .output()
        .unwrap();
    assert!(
        finish.status.success(),
        "{}",
        String::from_utf8_lossy(&finish.stderr)
    );
    let v: Value = serde_json::from_slice(&finish.stdout).unwrap();
    assert_eq!(v, json!({"id": id, "outcome": "done"}));

    let again = Command::new(BIN)
        .args(["--store", store.to_str().unwrap(), "call", "task_finish"])
        .arg(json!({"id": id, "outcome": "done"}).to_string())
        .output()
        .unwrap();
    assert_eq!(again.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&again.stderr).contains("already finished"));

    std::fs::remove_dir_all(store.parent().unwrap()).ok();
}
