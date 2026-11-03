//! Minimal MCP server over stdio: newline-delimited JSON-RPC 2.0.
//!
//! Implements what a tool-only server needs: `initialize`, `ping`, `tools/list`
//! and `tools/call`. Notifications are ignored. Logs go to stderr; stdout carries
//! protocol messages only. Exits when stdin closes.

use std::io::{BufRead, Write};

use anyhow::Result;
use serde_json::{Value, json};

use crate::store::Store;
use crate::tools::{self, ToolError};

/// Protocol revisions we can speak; the client's choice is echoed when known.
const KNOWN_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
const FALLBACK_VERSION: &str = "2025-06-18";

const INSTRUCTIONS: &str = concat!(
    "Reports work to the hermes-beaver wall dashboard. Call task_start when you begin real work ",
    "for the owner, task_update with waiting_feedback when blocked on them, and task_finish when done. ",
    "Never include secrets in any text."
);

pub fn serve(store: &mut Store) -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = handle_line(store, &line) {
            serde_json::to_writer(&mut stdout, &reply)?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

/// Handles one message; returns the response, or `None` for notifications.
pub fn handle_line(store: &mut Store, line: &str) -> Option<Value> {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return Some(error(Value::Null, -32700, &format!("parse error: {e}"))),
    };
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str);
    let (Some(id), Some(method)) = (id, method) else {
        // Notification (no id) or a response to something we never sent.
        return None;
    };
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    Some(match method {
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(FALLBACK_VERSION);
            let version = if KNOWN_VERSIONS.contains(&requested) {
                requested
            } else {
                FALLBACK_VERSION
            };
            result(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "hermes-beaver", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": INSTRUCTIONS
                }),
            )
        }
        "ping" => result(id, json!({})),
        "tools/list" => {
            let tools: Vec<Value> = tools::definitions()
                .into_iter()
                .map(|t| {
                    json!({"name": t.name, "description": t.description, "inputSchema": t.input_schema})
                })
                .collect();
            result(id, json!({"tools": tools}))
        }
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match tools::call(store, name, args) {
                Ok(out) => result(
                    id,
                    json!({
                        "content": [{"type": "text", "text": out.to_string()}],
                        "structuredContent": out,
                        "isError": false
                    }),
                ),
                Err(ToolError::UnknownTool(n)) => error(id, -32602, &format!("unknown tool `{n}`")),
                Err(e) => {
                    if let ToolError::Internal(err) = &e {
                        eprintln!("hermes-beaver mcp: {name}: {err:#}");
                    }
                    result(
                        id,
                        json!({
                            "content": [{"type": "text", "text": e.to_string()}],
                            "isError": true
                        }),
                    )
                }
            }
        }
        other => error(id, -32601, &format!("method not found: {other}")),
    })
}

fn result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn send(s: &mut Store, msg: Value) -> Option<Value> {
        handle_line(s, &msg.to_string())
    }

    #[test]
    fn handshake_list_and_call() {
        let mut s = Store::open_in_memory().unwrap();
        let init = send(
            &mut s,
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                   "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}}),
        )
        .unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert!(init["result"]["capabilities"]["tools"].is_object());

        assert!(
            send(
                &mut s,
                json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
            )
            .is_none()
        );

        let list = send(
            &mut s,
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        )
        .unwrap();
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "task_start",
                "task_update",
                "task_finish",
                "queue_set",
                "task_closed"
            ]
        );

        let call = send(
            &mut s,
            json!({"jsonrpc": "2.0", "id": "a", "method": "tools/call",
                   "params": {"name": "task_start", "arguments": {"source": "telegram", "title": "hello"}}}),
        )
        .unwrap();
        assert_eq!(call["id"], "a");
        assert_eq!(call["result"]["isError"], false);
        assert_eq!(call["result"]["structuredContent"]["id"], 1);
    }

    #[test]
    fn tool_errors_are_results_not_protocol_errors() {
        let mut s = Store::open_in_memory().unwrap();
        let r = send(
            &mut s,
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                   "params": {"name": "task_start", "arguments": {"source": "x", "title": "y"}}}),
        )
        .unwrap();
        assert_eq!(r["result"]["isError"], true);
        assert!(
            r["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("source must be")
        );

        let unknown = send(
            &mut s,
            json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "nope"}}),
        )
        .unwrap();
        assert_eq!(unknown["error"]["code"], -32602);
    }

    #[test]
    fn unknown_version_falls_back_and_bad_input_is_reported() {
        let mut s = Store::open_in_memory().unwrap();
        let init = send(
            &mut s,
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "1999-01-01"}}),
        )
        .unwrap();
        assert_eq!(init["result"]["protocolVersion"], FALLBACK_VERSION);
        assert_eq!(
            handle_line(&mut s, "{not json").unwrap()["error"]["code"],
            -32700
        );
        let m = send(
            &mut s,
            json!({"jsonrpc": "2.0", "id": 9, "method": "resources/list"}),
        )
        .unwrap();
        assert_eq!(m["error"]["code"], -32601);
        assert_eq!(
            send(&mut s, json!({"jsonrpc": "2.0", "id": 5, "method": "ping"})).unwrap()["result"],
            json!({})
        );
    }
}
