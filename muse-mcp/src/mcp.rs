//! Minimal Model Context Protocol server over stdio (JSON-RPC 2.0, newline
//! delimited). Implements `initialize`, `tools/list` and `tools/call`, which is
//! the subset every MCP client needs to drive these tools.

use crate::client::MuseClient;
use anyhow::Result;
use serde_json::{json, Value};
use std::io::BufRead;
use tokio::io::AsyncWriteExt;
use std::sync::Arc;

pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// Handshake text handed to every MCP client. It states the operating
/// constraints a caller needs before it starts choosing tools, because a wrong
/// first assumption here costs a wasted turn: which transport carries what,
/// where credentials live, and which tools degrade to agent mediation.
pub const MUSE_INSTRUCTIONS: &str = "\
Muse (Meta personal agent, com.facebook.aura / Hatch) tools.

Auth: the session is reused from a state file (Windows %APPDATA%/muse-mcp, else \
$XDG_CONFIG_HOME/muse-mcp or ~/.config/muse-mcp, override with MUSE_MCP_STATE). Call \
muse_status first; only when it reports signed_in=false do you need muse_login_start then \
muse_login_verify with a code the user reads back.

Transports behind the tools: /api/* on muse.ai with browser cookies, /hatch/* on \
hatch-api.meta.ai with a bearer token, and a Noise-encrypted WebSocket to the VM for \
chat, goals, and the file API. Chat replies are not returned by the submit call, so \
muse_ask and muse_relay poll history until the turn finishes.

Files: /fs/* is confined to the VM workspace and rejects absolute paths and traversal. \
Paths are workspace-relative: pass 'AGENTS.md', not '/workspace/AGENTS.md'. For \
/etc, /proc, /var use muse_sys or muse_exec, which run through the agent inside the VM \
and report transport=agent-shell. Their cost is one agent turn each, so prefer the \
direct file API when the target is inside the workspace.

Sub-chats: thread ids are derived from the name, so the same name always resumes the \
same conversation. An unknown name is not created implicitly.

Relay turns (DSH<->Muse): muse_relay puts museSessionId, dshSessionId, and requestId \
on the wire and also reuses requestId as the transport x-request-id. The requestId is \
not stored in the gateway transcript; correlate with the returned sent_message_id.

Destructive calls (muse_rpc DELETE/PUT, muse_goals delete, muse_session_action delete) \
need confirm=true. muse_fs_delete does not ask, and there is no restore route.";

/// Serialised access to stdout.
///
/// Two writers exist: the read loop (immediate replies) and spawned tool
/// tasks (slow ones). A JSON-RPC message must occupy one line, so writes are
/// serialised rather than interleaved.
type SharedWriter = Arc<tokio::sync::Mutex<tokio::io::Stdout>>;

pub struct Server {
    client: Arc<MuseClient>,
}

impl Server {
    pub fn new(client: Arc<MuseClient>) -> Self {
        Self { client }
    }

    pub async fn serve_stdio(&self) -> Result<()> {
        let stdin = std::io::stdin();
        let stdout: SharedWriter = Arc::new(tokio::sync::Mutex::new(tokio::io::stdout()));
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
                    write_line(&stdout, &response.to_string()).await;
                    continue;
                }
            };
            let id = request.get("id").cloned();
            let method = request.get("method").and_then(Value::as_str).unwrap_or_default();
            let params = request.get("params").cloned().unwrap_or(Value::Null);

            if let Some(outcome) = self.dispatch(&method, params, &id, &stdout).await {
                let Some(id) = id else {
                    // Notification: no response is written.
                    continue;
                };
                let response = match outcome {
                    Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                    Err((code, message)) => error_response(id, code, &message),
                };
                write_line(&stdout, &response.to_string()).await;
            }
        }
        Ok(())
    }

    /// Run one request and return its outcome, or `None` when the request
    /// needs no response on this loop (a notification, or a tool call that was
    /// handed to a spawned task).
    async fn dispatch(
        &self,
        method: &str,
        params: Value,
        id: &Option<Value>,
        stdout: &SharedWriter,
    ) -> Option<Result<Value, (i64, String)>> {
        match method {
            "initialize" => Some(Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": {
                    "name": "muse-mcp",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "instructions": MUSE_INSTRUCTIONS,
            }))),
            "notifications/initialized" | "initialized" => None,
            "ping" => Some(Ok(json!({}))),
            "tools/list" => Some(Ok(json!({ "tools": crate::tools::definitions() }))),
            "tools/call" => {
                // A tool turn can outlive any sensible client timeout: a relay
                // waits for the assistant, muse_exec waits for an agent turn.
                // Running it on this loop meant one hung call blocked every
                // later call until the client gave up, so callers saw a
                // timeout on tools that were never even started. Run it off
                // the loop and let the client correlate by id.
                let Some(id) = id.clone() else {
                    return Some(Err((-32600, "tools/call requires an id".to_string())));
                };
                let client = self.client.clone();
                let stdout = stdout.clone();
                tokio::spawn(async move {
                    let outcome = call_tool(&client, params).await;
                    let response = match outcome {
                        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                        Err((code, message)) => error_response(id, code, &message),
                    };
                    write_line(&stdout, &response.to_string()).await;
                });
                None
            }
            other => Some(Err((-32601, format!("method not found: {other}"))))
        }
    }

}

/// Run one tool and wrap the result in the MCP content envelope.
async fn call_tool(
    client: &Arc<MuseClient>,
    params: Value,
) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or((-32602, "tools/call requires a tool name".to_string()))?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    match crate::tools::call(client, name, arguments).await {
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

/// Write one JSON-RPC message as a single line.
async fn write_line(stdout: &SharedWriter, line: &str) {
    let mut out = stdout.lock().await;
    let _ = out.write_all(line.as_bytes()).await;
    let _ = out.write_all(b"\n").await;
    let _ = out.flush().await;
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}
