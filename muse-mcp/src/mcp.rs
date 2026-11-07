//! Minimal Model Context Protocol server over stdio (JSON-RPC 2.0, newline
//! delimited). Implements `initialize`, `tools/list` and `tools/call`, which is
//! the subset every MCP client needs to drive these tools.

use crate::client::MuseClient;
use anyhow::Result;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::sync::Arc;

pub const PROTOCOL_VERSION: &str = "2024-11-05";

pub struct Server {
    client: Arc<MuseClient>,
}

impl Server {
    pub fn new(client: Arc<MuseClient>) -> Self {
        Self { client }
    }

    pub async fn serve_stdio(&self) -> Result<()> {
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        let mut line = String::new();
        loop {
            line.clear();
            let read = stdin.lock().read_line(&mut line)?;
            if read == 0 {
                break;
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let request: Value = match serde_json::from_str(trimmed) {
                Ok(value) => value,
                Err(err) => {
                    let response = error_response(Value::Null, -32700, &format!("parse error: {err}"));
                    writeln!(stdout, "{response}")?;
                    stdout.flush()?;
                    continue;
                }
            };
            let id = request.get("id").cloned();
            let method = request.get("method").and_then(Value::as_str).unwrap_or_default();
            let params = request.get("params").cloned().unwrap_or(Value::Null);

            let outcome = match method {
                "initialize" => Ok(json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": {
                        "name": "muse-mcp",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "instructions": "Muse (Meta personal agent) tools. Call muse_login_start then \
                        muse_login_verify once; the session is reused from the state file afterwards.",
                })),
                "notifications/initialized" | "initialized" => {
                    continue;
                }
                "ping" => Ok(json!({})),
                "tools/list" => Ok(json!({ "tools": crate::tools::definitions() })),
                "tools/call" => self.call_tool(params).await,
                other => Err((-32601, format!("method not found: {other}"))),
            };

            let Some(id) = id else {
                // Notification: no response is written.
                continue;
            };
            let response = match outcome {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Err((code, message)) => error_response(id, code, &message),
            };
            writeln!(stdout, "{response}")?;
            stdout.flush()?;
        }
        Ok(())
    }

    async fn call_tool(&self, params: Value) -> Result<Value, (i64, String)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or((-32602, "tools/call requires a tool name".to_string()))?;
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        match crate::tools::call(&self.client, name, arguments).await {
            Ok(payload) => Ok(json!({
                "content": [{ "type": "text", "text": payload }],
                "isError": false,
            })),
            Err(err) => Ok(json!({
                "content": [{ "type": "text", "text": format!("{}", err) }],
                "isError": true,
            })),
        }
    }
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}
