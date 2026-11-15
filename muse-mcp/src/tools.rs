//! MCP tool catalogue and dispatch.
//!
//! Tool outputs are always JSON text so a calling model can parse them without
//! a second protocol layer.

use crate::client::MuseClient;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "muse_status",
            "description": "Report whether a Muse session is stored, whether it still validates, and where the state file lives.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }),
        json!({
            "name": "muse_login_start",
            "description": "Begin Muse OTP login: primes the browser session and sends a one-time code to an email address or phone number.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "contact_point": { "type": "string", "description": "Email address or E.164 phone number registered with Meta" },
                    "region": { "type": "string", "description": "Two-letter region hint for the OTP delivery (default US)" }
                },
                "required": ["contact_point"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_login_verify",
            "description": "Complete Muse OTP login with the code the user received. Saves the session for later calls.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "otp_code": { "type": "string", "description": "One-time code from the email or SMS" }
                },
                "required": ["otp_code"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_logout",
            "description": "Sign out of Muse and delete the stored session cookies.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }),
        json!({
            "name": "muse_approvals",
            "description": "List, inspect, or decide the agent approvals that gate tool execution inside Muse. Approvals live on the sentinel gateway service, not the daemon; every /approvals route 404s on the daemon and 200s on service 1. Each entry carries approval_id, host, path, method, decision and decision_reason. Decisions use the lowercase enum allow_once / allow_session / allow_today / allow_always / auto_allow / deny / deny_always. Anything other than allow_once or deny grants access past the request already waiting, so it needs confirm=true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "show", "decide"], "description": "Defaults to list" },
                    "approval_id": { "type": "string", "description": "Required for show and decide" },
                    "decision": {
                        "type": "string",
                        "enum": ["allow_once", "allow_session", "allow_today", "allow_always", "auto_allow", "deny", "deny_always"],
                        "description": "Required for decide"
                    },
                    "confirm": { "type": "boolean", "description": "Required for any decision other than allow_once or deny" },
                    "limit": { "type": "integer", "description": "Maximum recent entries to list (default 20)", "minimum": 1 }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_ping",
            "description": "Round-trip the Hatch gateway over the encrypted channel to verify the VM connection.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }),
        json!({
            "name": "muse_model",
            "description": "Read the agent model descriptor advertised by the Hatch VM.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }),
        json!({
            "name": "muse_sessions",
            "description": "List Muse conversation sessions for the signed-in account.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }),
        json!({
            "name": "muse_history",
            "description": "Fetch conversation history (messages, tool calls, media references) for a session or the primary chat.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Session id from muse_sessions; omit for the primary chat" },
                    "limit": { "type": "integer", "description": "Maximum number of transcript entries (default 40)", "minimum": 1, "maximum": 500 }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_ask",
            "description": "Send a prompt to Muse and wait for the assistant reply. Text plus optional image attachments. Set wait_for_reply=false to only get the submission acknowledgement.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "Message to send to the agent" },
                    "session_id": { "type": "string", "description": "Continue an existing session; omit for the primary chat" },
                    "attachments": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Absolute paths to PNG/JPEG/WebP/GIF files to attach"
                    },
                    "wait_for_reply": { "type": "boolean", "description": "Poll history for the assistant reply (default true)" },
                    "reply_timeout_sec": { "type": "integer", "description": "How long to wait for the reply (default 180)", "minimum": 5, "maximum": 1800 }
                },
                "required": ["prompt"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_relay",
            "description": "Send one prompt to Muse on the relay path. The server resolves museSessionId, dshSessionId, and requestId, refuses anything it cannot attribute, and prepends a [muse-relay] block to the body itself, matching the format Muse uses toward DSH; the same three values also go on the wire as headers. Do not write a [muse-relay] block into prompt: a block contradicting the resolved values is an error, and an identical one is stripped. museSessionId is required and must be a lower-case UUID. dshSessionId comes from DSH_SESSION_ID when omitted and is checked against the argument when both exist. requestId is kept verbatim for replies and generated as dsh-<uuid> when absent. Each delivery is recorded in relay-audit.jsonl next to the session state, with the three ids and the outcome but not the body.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "Message to deliver to the Muse agent" },
                    "museSessionId": { "type": "string", "description": "Required. The Muse conversation the message is delivered to, as a lower-case UUID. A name or a malformed id is refused before delivery." },
                    "dshSessionId": { "type": "string", "description": "The sending DSH session. Filled from DSH_SESSION_ID when omitted; a value that disagrees with it is refused." },
                    "requestId": { "type": "string", "description": "Correlation id. Reply to a Muse-originated request with the requestId you received; omit it and the server generates a unique dsh- id." },
                    "attachments": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Absolute paths to PNG/JPEG/WebP/GIF files to attach"
                    },
                    "reply_timeout_sec": { "type": "integer", "description": "How long to wait for the reply (default 180)", "minimum": 5, "maximum": 1800 }
                },
                "required": ["prompt"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_cancel",
            "description": "Cancel an in-flight agent turn for a session.",
            "inputSchema": {
                "type": "object",
                "properties": { "session_id": { "type": "string" } },
                "required": ["session_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_fs_list",
            "description": "List a directory inside the Muse VM workspace.",
            "inputSchema": {
                "type": "object",
                "properties": { "path": { "type": "string", "description": "Workspace-relative path (empty for the workspace root)" } },
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_fs_stat",
            "description": "Stat one path inside the Muse VM workspace.",
            "inputSchema": {
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_fs_read",
            "description": "Read a file from the Muse VM workspace. Text comes back inline as utf8. A window that does not start on a UTF-8 character boundary returns base64 even for a text file, so shift offset by a byte or two when that happens.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": "integer", "description": "Start byte offset (default 0)", "minimum": 0 },
                    "max_bytes": { "type": "integer", "description": "Byte cap for the read (default 262144)", "minimum": 1, "maximum": 8388608 },
                    "encoding": { "type": "string", "enum": ["utf8", "base64"], "description": "How to render the bytes (default utf8)" }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_fs_write",
            "description": "Write a text file into the Muse VM workspace, creating parents as needed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "text": { "type": "string" },
                    "append": { "type": "boolean", "description": "Append instead of overwriting (default false)" }
                },
                "required": ["path", "text"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_fs_upload",
            "description": "Upload a local file into the Muse VM workspace in 512 KiB chunks with staged rename for multi-chunk transfers. Same operation as muse_fs_put; both names exist for callers written against either one.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "local_path": { "type": "string", "description": "Absolute path on this machine" },
                    "remote_path": { "type": "string", "description": "Workspace-relative destination" }
                },
                "required": ["local_path", "remote_path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_fs_delete",
            "description": "Delete a path inside the Muse VM workspace. The gateway has no restore route, so deletion is final. Use recursive=true only to remove a whole directory. Takes effect immediately and does not ask for confirmation.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "recursive": { "type": "boolean", "description": "Delete directories recursively (default false)" }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_fs_rename",
            "description": "Move or rename a path inside the Muse VM workspace.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "source": { "type": "string" },
                    "destination": { "type": "string" }
                },
                "required": ["source", "destination"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_fs_search",
            "description": "Search filenames and extensions inside the Muse VM workspace.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Substring of the file name (optional if extensions given)" },
                    "extensions": { "type": "array", "items": { "type": "string" }, "description": "Extensions to match, e.g. [\"md\", \"txt\"]" }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_fs_library",
            "description": "Browse the file library presets: recents, media, artifacts.",
            "inputSchema": {
                "type": "object",
                "properties": { "preset": { "type": "string", "enum": ["recents", "media", "artifacts"] } },
                "required": ["preset"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_fs_mkdir",
            "description": "Create a directory inside the Muse VM workspace.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "recursive": { "type": "boolean", "description": "Create parents (default true)" }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_fs_download",
            "description": "Download a workspace file to this machine byte-exactly (binary safe).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "remote_path": { "type": "string" },
                    "local_path": { "type": "string", "description": "Absolute destination path" }
                },
                "required": ["remote_path", "local_path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_fs_put",
            "description": "Upload a local file into the workspace in 512 KiB chunks (staged rename for multi-chunk). Same operation as muse_fs_upload.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "local_path": { "type": "string", "description": "Absolute source path" },
                    "remote_path": { "type": "string", "description": "Workspace-relative destination" }
                },
                "required": ["local_path", "remote_path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_raw",
            "description": "Fetch any gateway path that answers with bytes (e.g. /fs/export) and save it locally. Use this when the body is not JSON.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "method": { "type": "string", "enum": ["GET", "POST"], "description": "Defaults to GET" },
                    "path": { "type": "string" },
                    "params": { "type": "object" },
                    "local_path": { "type": "string", "description": "Where to write the bytes" },
                    "timeout_sec": { "type": "integer" }
                },
                "required": ["path", "local_path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_account_get",
            "description": "Call a read-only Muse account REST endpoint (hatch-api.meta.ai). Example paths: /hatch/viewer/profile, /hatch/activation_statuses, /hatch/tos_status.",
            "inputSchema": {
                "type": "object",
                "properties": { "path": { "type": "string", "description": "Absolute API path beginning with /hatch/" } },
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_account_post",
            "description": "Call a Muse account REST endpoint with a JSON body. Only non-destructive account operations are intended.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "body": { "type": "object", "description": "JSON request body" }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_goals",
            "description": "Manage goals: list, get, create, update, delete. create requires title, summary, description. update accepts title/summary/description/status/emoji/momentum. delete needs confirm=true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "get", "create", "update", "delete"], "description": "Defaults to list" },
                    "goal_id": { "type": "string", "description": "goal_… id for get/update/delete" },
                    "title": { "type": "string" },
                    "summary": { "type": "string" },
                    "description": { "type": "string" },
                    "emoji": { "type": "string" },
                    "status": { "type": "string", "enum": ["active", "paused", "retired", "completed"] },
                    "momentum": { "type": "string" },
                    "include_archived": { "type": "boolean" },
                    "confirm": { "type": "boolean", "description": "Required for delete" }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_rpc",
            "description": "Call any Hatch gateway route directly (escape hatch). GET/POST/PUT/PATCH/DELETE. Verified readable: /api/goals, /api/activity, /api/artifacts, /api/connectors, /api/tasks, /api/feed, /api/identity, /api/chat/*, /api/session/*, /api/voice/*, /api/idea-cards, /api/onboarding/*, /api/navigation/tab-order, /api/product-improvements, /api/podcasts/feed, /api/computer/context. Closed to this client by the endpoint ACL (403 matched route missing from endpoint ACL): POST /api/tasks/shell, POST /api/connectors, POST /api/navigation/tab-order, /api/browser/lease, /api/feed/prompt, /api/artifacts/placement, /api/artifacts/save, /api/chat/delete-turn. Do not exist (404): /api/approvals*, /api/permissions/*, /api/spaces*, /api/exec, /api/terminal, /api/shell. Destructive verbs require confirm=true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "method": { "type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE"] },
                    "path": { "type": "string", "description": "Gateway path beginning with '/' (query string allowed)" },
                    "params": { "type": "object", "description": "Query parameters for GET, JSON body otherwise" },
                    "confirm": { "type": "boolean", "description": "Acknowledge destructive DELETE/PUT calls" }
                },
                "required": ["method", "path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_web_api",
            "description": "Call any muse.ai browser API route directly with the stored cookies (escape hatch). Covers routes the mobile client does not use, e.g. /api/hatch/vm/wake, /api/hatch/token, /api/client-version, /api/consent/status, /api/logout.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "method": { "type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE"] },
                    "path": { "type": "string", "description": "API path beginning with '/'" },
                    "body": { "type": "object", "description": "JSON body for non-GET methods" },
                    "use_csrf": { "type": "boolean", "description": "Attach x-hatch-csrf-token (needed by /api/auth/native/*)" }
                },
                "required": ["method", "path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_session_action",
            "description": "Change a conversation session: rename, pin, unpin, archive, unarchive, delete.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["rename", "pin", "unpin", "archive", "unarchive", "delete"] },
                    "session_id": { "type": "string" },
                    "title": { "type": "string", "description": "New title (rename only)" },
                    "confirm": { "type": "boolean", "description": "Required for delete" }
                },
                "required": ["action", "session_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_threads",
            "description": "List the primary conversation and every side thread (sub-chat), newest first.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }),
        json!({
            "name": "muse_thread",
            "description": "Create or manage a named sub-chat. Thread ids are derived from the name, so the same name always resumes the same sub-chat. Keeps experiments out of the primary conversation.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["new", "rename", "pin", "unpin", "archive", "unarchive", "delete"], "description": "Defaults to new" },
                    "name": { "type": "string", "description": "Thread name used with --thread" },
                    "prompt": { "type": "string", "description": "Opening prompt when creating" },
                    "title": { "type": "string", "description": "New title for rename" },
                    "confirm": { "type": "boolean", "description": "Required for delete" },
                    "timeout_sec": { "type": "integer" }
                },
                "required": ["name"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_exec",
            "description": "Run a shell command inside the Muse VM through the agent. Returns fenced stdout/stderr plus exit code. This is agent-mediated (root shell), not a direct file API; every result carries transport=agent-shell.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to run" },
                    "thread": { "type": "string", "description": "Run inside this named sub-chat" },
                    "timeout_sec": { "type": "integer", "description": "Default 240" }
                },
                "required": ["command"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_sys",
            "description": "Read system paths (cat/ls/find) or run a raw command inside the VM through the agent. The gateway file API rejects absolute paths, so this is the only route to /etc, /proc, /var and friends.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "verb": { "type": "string", "enum": ["cat", "ls", "find", "raw"] },
                    "target": { "type": "string", "description": "Path for cat/ls/find, command for raw" },
                    "thread": { "type": "string", "description": "Run inside this named sub-chat" },
                    "timeout_sec": { "type": "integer" }
                },
                "required": ["verb", "target"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "muse_settings",
            "description": "Read or change agent settings. action=read returns eight sections: model, identity, tab_order, product_improvements, voice_active, chat_themes, onboarding_tasks, onboarding_variants. The gateway has no permissions endpoint, so permissions cannot be read or set here. set_tab_order and set_voice are the only writable actions.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["read", "set_tab_order", "set_voice"], "description": "Defaults to read" },
                    "tab_order": { "type": "array", "items": { "type": "string" }, "description": "Ordered tab identifiers for set_tab_order" },
                    "voice_id": { "type": "string", "description": "Voice identifier for set_voice" }
                },
                "additionalProperties": false
            }
        }),
    ]
}

pub async fn call(client: &Arc<MuseClient>, name: &str, args: Value) -> Result<String> {
    match name {
        "muse_status" => {
            let mut http = client.http_mut().await;
            let check = http.auth_check().await?;
            Ok(pretty(json!({
                "state_path": client.state_path().display().to_string(),
                "signed_in": check.get("ok").and_then(Value::as_bool).unwrap_or(false),
                "auth_check": check,
            })))
        }
        "muse_login_start" => {
            let contact = required_str(&args, "contact_point")?;
            let region = args
                .get("region")
                .and_then(Value::as_str)
                .unwrap_or("US")
                .to_string();
            let mut http = client.http_mut().await;
            http.restart_login().await?;
            let sent = http.send_otp(&contact, &region).await?;
            Ok(pretty(json!({
                "contact_point": contact,
                "region": region,
                "result": sent,
                "next": "Call muse_login_verify with the code that arrived.",
            })))
        }
        "muse_login_verify" => {
            let code = required_str(&args, "otp_code")?;
            let mut http = client.http_mut().await;
            let confirmed = http.confirm_otp(&code).await?;
            let saved = http.save_account().await.ok();
            let check = http.auth_check().await?;
            let ok = check.get("ok").and_then(Value::as_bool).unwrap_or(false);
            if !ok {
                bail!(
                    "Muse accepted the code but the session did not validate: {}",
                    pretty(check.clone())
                );
            }
            Ok(pretty(json!({
                "signed_in": true,
                "confirm": confirmed,
                "save_account": saved,
                "auth_check": check,
            })))
        }
        "muse_logout" => {
            let mut http = client.http_mut().await;
            client.disconnect().await;
            let result = http.logout().await?;
            Ok(pretty(json!({ "signed_out": true, "result": result })))
        }
        "muse_approvals" => {
            let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
            match action {
                "list" => {
                    let snapshot = client.approvals().await?;
                    let limit = args
                        .get("limit")
                        .and_then(Value::as_u64)
                        .unwrap_or(20) as usize;
                    let pending: Vec<Value> = snapshot
                        .get("pending")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let recent: Vec<Value> = snapshot
                        .get("recent")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .take(limit)
                        .collect();
                    Ok(pretty(json!({
                        "emitted_at_ms": snapshot.get("emitted_at_ms").cloned().unwrap_or(Value::Null),
                        "next_cursor": snapshot.get("next_cursor").cloned().unwrap_or(Value::Null),
                        "pending_count": pending.len(),
                        "pending": pending,
                        "recent": recent,
                    })))
                }
                "show" => {
                    let approval_id = required_str(&args, "approval_id")?;
                    let snapshot = client.approvals().await?;
                    let mut found = None;
                    for key in ["pending", "pending_approvals", "recent"] {
                        let Some(list) = snapshot.get(key).and_then(Value::as_array) else {
                            continue;
                        };
                        if let Some(entry) = list.iter().find(|entry| {
                            entry.get("approval_id").and_then(Value::as_str)
                                == Some(approval_id.as_str())
                        }) {
                            found = Some(entry.clone());
                            break;
                        }
                    }
                    let approval =
                        found.ok_or_else(|| anyhow!("no approval with id {approval_id}"))?;
                    Ok(pretty(approval))
                }
                "decide" => {
                    let approval_id = required_str(&args, "approval_id")?;
                    let decision = required_str(&args, "decision")?;
                    if !crate::DECISIONS.contains(&decision.as_str()) {
                        bail!(
                            "unknown decision `{decision}`; expected one of {}",
                            crate::DECISIONS.join(", ")
                        );
                    }
                    let passive = matches!(decision.as_str(), "allow_once" | "deny");
                    if !passive && !args.get("confirm").and_then(Value::as_bool).unwrap_or(false) {
                        bail!(
                            "{decision} grants access beyond this one call; pass confirm=true. \
                             Use allow_once for a single request."
                        );
                    }
                    let result = client
                        .decide_approvals(json!([
                            { "approval_id": approval_id, "decision": decision }
                        ]))
                        .await?;
                    Ok(pretty(crate::decide_summary(&result)))
                }
                other => bail!("unknown approvals action {other}; expected list/show/decide"),
            }
        }
        "muse_ping" => Ok(pretty(client.ping().await?)),
        "muse_model" => Ok(pretty(client.model().await?)),
        "muse_sessions" => Ok(pretty(client.sessions().await?)),
        "muse_history" => {
            let session_id = args.get("session_id").and_then(Value::as_str);
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(40) as u32;
            Ok(pretty(client.history(session_id, limit).await?))
        }
        "muse_relay" => {
            let prompt = required_str(&args, "prompt")?;
            // The server owns the header: it resolves the ids, refuses anything
            // it cannot attribute, and prepends the block itself. Nothing here
            // trusts the caller or the model to write one.
            let envelope = crate::relay::RelayEnvelope::resolve(
                args.get("museSessionId").and_then(Value::as_str),
                args.get("dshSessionId").and_then(Value::as_str),
                args.get("requestId").and_then(Value::as_str),
                &prompt,
            )?;
            let headers = envelope.to_headers()?;
            let timeout = args
                .get("reply_timeout_sec")
                .and_then(Value::as_u64)
                .unwrap_or(180);
            let attachments: Vec<PathBuf> = args
                .get("attachments")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(PathBuf::from)
                        .collect()
                })
                .unwrap_or_default();
            let started = std::time::Instant::now();
            let outcome = client
                .ask_and_wait_with_headers(
                    &envelope.body,
                    Some(&envelope.muse_session_id),
                    &attachments,
                    std::time::Duration::from_secs(timeout.clamp(5, 1800)),
                    std::time::Duration::from_secs(2),
                    &headers,
                )
                .await;

            let (delivered, detail) = match &outcome {
                Ok(outcome) => (true, outcome.sent_message_id.clone()),
                Err(err) => (false, Some(err.to_string())),
            };
            crate::relay::write_audit(
                client.state_path(),
                &json!({
                    "at_ms": crate::relay::now_ms(),
                    "tool": "muse_relay",
                    "museSessionId": envelope.muse_session_id,
                    "dshSessionId": envelope.dsh_session_id,
                    "requestId": envelope.request_id,
                    "delivered": delivered,
                    "detail": detail,
                }),
            );

            let outcome = outcome?;
            let text = outcome
                .reply
                .get("display_text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Ok(pretty(json!({
                "relay": envelope.to_json(),
                "session_id": outcome.session_id,
                "sent_message_id": outcome.sent_message_id,
                "replied": !text.is_empty(),
                "text": text,
                "elapsed_ms": started.elapsed().as_millis() as u64,
            })))
        }
        "muse_ask" => {
            let prompt = required_str(&args, "prompt")?;
            let session_id = args.get("session_id").and_then(Value::as_str);
            let wait = args.get("wait_for_reply").and_then(Value::as_bool).unwrap_or(true);
            let timeout = args
                .get("reply_timeout_sec")
                .and_then(Value::as_u64)
                .unwrap_or(180);
            let attachments: Vec<PathBuf> = args
                .get("attachments")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(PathBuf::from)
                        .collect()
                })
                .unwrap_or_default();

            if !wait {
                let events = client
                    .ask(
                        &prompt,
                        session_id,
                        &attachments,
                        std::time::Duration::from_secs(45),
                    )
                    .await?;
                return Ok(pretty(json!({
                    "event_count": events.len(),
                    "events": events,
                    "note": "wait_for_reply=false: only the submission acknowledgement is reported",
                })));
            }

            let outcome = client
                .ask_and_wait(
                    &prompt,
                    session_id,
                    &attachments,
                    std::time::Duration::from_secs(timeout),
                    std::time::Duration::from_secs(2),
                )
                .await?;
            let replied = !outcome.reply.is_null();
            Ok(pretty(json!({
                "session_id": outcome.session_id,
                "sent_message_id": outcome.sent_message_id,
                "replied": replied,
                "reply": outcome.reply,
                "ack": outcome.events,
                "history": if replied { Value::Null } else { outcome.history.unwrap_or(Value::Null) },
            })))
        }
        "muse_cancel" => {
            let session_id = required_str(&args, "session_id")?;
            Ok(pretty(client.cancel_stream(&session_id).await?))
        }
        "muse_fs_list" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or("");
            Ok(pretty(client.fs_list(path).await?))
        }
        "muse_fs_stat" => {
            let path = required_str(&args, "path")?;
            Ok(pretty(client.fs_stat(&path).await?))
        }
        "muse_fs_read" => {
            let path = required_str(&args, "path")?;
            if let Some(offset) = args.get("offset").and_then(Value::as_u64) {
                if offset > 0 {
                    let max_bytes = args
                        .get("max_bytes")
                        .and_then(Value::as_u64)
                        .unwrap_or(262_144) as usize;
                    let (bytes, text) = client.fs_read_window(&path, offset, max_bytes).await?;
                    let encoding = args.get("encoding").and_then(Value::as_str).unwrap_or("utf8");
                    let rendered = if encoding == "base64" || text.is_none() {
                        crate::client::base64_encode(&bytes)
                    } else {
                        text.unwrap_or_default()
                    };
                    return Ok(pretty(json!({
                        "path": path,
                        "offset": offset,
                        "bytes": bytes.len(),
                        "encoding": if encoding == "base64" || rendered.len() == 0 { "base64" } else { "utf8" },
                        "content": rendered,
                    })));
                }
            }
            let max_bytes = args
                .get("max_bytes")
                .and_then(Value::as_u64)
                .unwrap_or(262_144) as usize;
            let encoding = args
                .get("encoding")
                .and_then(Value::as_str)
                .unwrap_or("utf8");
            let (bytes, truncated) = client.fs_read(&path, max_bytes).await?;
            let rendered = match encoding {
                "base64" => crate::client::base64_encode(&bytes),
                _ => match String::from_utf8(bytes.clone()) {
                    Ok(text) => text,
                    Err(_) => {
                        return Ok(pretty(json!({
                            "path": path,
                            "encoding": "base64",
                            "note": "content is not valid UTF-8, returned as base64",
                            "truncated": truncated,
                            "content": crate::client::base64_encode(&bytes),
                        })))
                    }
                },
            };
            Ok(pretty(json!({
                "path": path,
                "encoding": encoding,
                "bytes": bytes.len(),
                "truncated": truncated,
                "content": rendered,
            })))
        }
        "muse_fs_write" => {
            let path = required_str(&args, "path")?;
            let text = required_str(&args, "text")?;
            let append = args
                .get("append")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Ok(pretty(client.fs_write(&path, text.as_bytes(), append).await?))
        }
        "muse_fs_upload" => {
            let local = required_str(&args, "local_path")?;
            let remote = required_str(&args, "remote_path")?;
            Ok(pretty(client.fs_upload(&PathBuf::from(local), &remote).await?))
        }
        "muse_fs_delete" => {
            let path = required_str(&args, "path")?;
            let recursive = args
                .get("recursive")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Ok(pretty(client.fs_delete(&path, recursive).await?))
        }
        "muse_fs_rename" => {
            let source = required_str(&args, "source")?;
            let destination = required_str(&args, "destination")?;
            Ok(pretty(client.fs_rename(&source, &destination).await?))
        }
        "muse_fs_search" => {
            let query = args.get("query").and_then(Value::as_str).unwrap_or("");
            let extensions: Vec<String> = args
                .get("extensions")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            Ok(pretty(client.fs_search(query, &extensions).await?))
        }
        "muse_fs_library" => {
            let preset = required_str(&args, "preset")?;
            Ok(pretty(client.fs_library(&preset).await?))
        }
        "muse_fs_mkdir" => {
            let path = required_str(&args, "path")?;
            let recursive = args
                .get("recursive")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            Ok(pretty(client.fs_mkdir(&path, recursive).await?))
        }
        "muse_fs_download" => {
            let remote = required_str(&args, "remote_path")?;
            let local = required_str(&args, "local_path")?;
            let bytes = client.fs_download(&remote).await?;
            let target = std::path::PathBuf::from(&local);
            if let Some(parent) = target.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            std::fs::write(&target, &bytes)
                .map_err(|err| anyhow!("cannot write {local}: {err}"))?;
            Ok(pretty(json!({
                "remote_path": remote,
                "local_path": local,
                "bytes": bytes.len(),
            })))
        }
        "muse_fs_put" => {
            let local = required_str(&args, "local_path")?;
            let remote = required_str(&args, "remote_path")?;
            Ok(pretty(
                client.fs_upload(&std::path::PathBuf::from(local), &remote).await?,
            ))
        }
        "muse_raw" => {
            let path = required_str(&args, "path")?;
            let method = args
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("GET")
                .to_ascii_uppercase();
            let params = args.get("params").cloned().filter(|value| !value.is_null());
            let local = required_str(&args, "local_path")?;
            let timeout = args
                .get("timeout_sec")
                .and_then(Value::as_u64)
                .unwrap_or(600);
            let bytes = client
                .raw_bytes(
                    &method,
                    &path,
                    params,
                    std::time::Duration::from_secs(timeout.max(10)),
                )
                .await?;
            let target = std::path::PathBuf::from(&local);
            if let Some(parent) = target.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            std::fs::write(&target, &bytes)
                .map_err(|err| anyhow!("cannot write {local}: {err}"))?;
            Ok(pretty(json!({
                "method": method,
                "path": path,
                "local_path": local,
                "bytes": bytes.len(),
                "zip": bytes.starts_with(b"PK\x03\x04"),
            })))
        }
        "muse_account_get" => {
            let path = required_str(&args, "path")?;
            guard_account_path(&path)?;
            let http = client.http_mut().await;
            Ok(pretty(http.stefi_get(&path).await?))
        }
        "muse_account_post" => {
            let path = required_str(&args, "path")?;
            guard_account_path(&path)?;
            let body = args.get("body").cloned().unwrap_or_else(|| json!({}));
            let http = client.http_mut().await;
            Ok(pretty(http.stefi_post(&path, body).await?))
        }
        "muse_rpc" => {
            let method = required_str(&args, "method")?;
            let path = required_str(&args, "path")?;
            let params = args.get("params").cloned().filter(|value| !value.is_null());
            let confirmed = args.get("confirm").and_then(Value::as_bool).unwrap_or(false);
            if is_destructive(&method, &path) && !confirmed {
                bail!(
                    "{} {} can destroy data. Re-run with \"confirm\": true once you are sure \
                     which records it targets.",
                    method.to_ascii_uppercase(),
                    path
                );
            }
            Ok(pretty(client.raw_rpc(&method, &path, params).await?))
        }
        "muse_goals" => {
            let action = args
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("list");
            match action {
                "list" => {
                    let mut path = "/api/goals".to_string();
                    if args
                        .get("include_archived")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        path.push_str("?include_archived=true");
                    }
                    Ok(pretty(client.raw_rpc("GET", &path, None).await?))
                }
                "get" => {
                    let id = required_str(&args, "goal_id")?;
                    Ok(pretty(
                        client
                            .raw_rpc("GET", &format!("/api/goals/{id}"), None)
                            .await?,
                    ))
                }
                "create" => {
                    let body = json!({
                        "title": required_str(&args, "title")?,
                        "summary": required_str(&args, "summary")?,
                        "description": required_str(&args, "description")?,
                        "emoji": args.get("emoji").and_then(Value::as_str).unwrap_or("🎯"),
                    });
                    Ok(pretty(client.raw_rpc("POST", "/api/goals", Some(body)).await?))
                }
                "update" => {
                    let id = required_str(&args, "goal_id")?;
                    let mut body = serde_json::Map::new();
                    for key in ["title", "summary", "description", "status", "emoji", "momentum"] {
                        if let Some(value) = args.get(key) {
                            body.insert(key.to_string(), value.clone());
                        }
                    }
                    if body.is_empty() {
                        bail!("update needs at least one of title/summary/description/status/emoji/momentum");
                    }
                    Ok(pretty(
                        client
                            .raw_rpc("PATCH", &format!("/api/goals/{id}"), Some(Value::Object(body)))
                            .await?,
                    ))
                }
                "delete" => {
                    let id = required_str(&args, "goal_id")?;
                    if !args.get("confirm").and_then(Value::as_bool).unwrap_or(false) {
                        bail!(
                            "deleting {id} is irreversible; re-run with \"confirm\": true"
                        );
                    }
                    Ok(pretty(
                        client
                            .raw_rpc("DELETE", &format!("/api/goals/{id}"), None)
                            .await?,
                    ))
                }
                other => bail!("unknown goal action {other}; expected list/get/create/update/delete"),
            }
        }
        "muse_web_api" => {
            let method = required_str(&args, "method")?;
            let path = required_str(&args, "path")?;
            let body = args.get("body").cloned().filter(|value| !value.is_null());
            let csrf = args.get("use_csrf").and_then(Value::as_bool).unwrap_or(false);
            let mut http = client.http_mut().await;
            Ok(pretty(http.web_api(&method, &path, body, csrf).await?))
        }
        "muse_session_action" => {
            let action = required_str(&args, "action")?;
            let session_id = required_str(&args, "session_id")?;
            let path = match action.as_str() {
                "rename" => "/api/session/rename",
                "pin" => "/api/session/pin",
                "unpin" => "/api/session/unpin",
                "archive" => "/api/session/archive",
                "unarchive" => "/api/session/unarchive",
                "delete" => "/api/session/delete",
                other => bail!(
                    "unknown session action {other}; expected rename/pin/unpin/archive/unarchive/delete"
                ),
            };
            // pin / unpin / archive / unarchive / delete validate that the body
            // echoes the route in a `method` field; rename does not.
            let mut body = json!({ "session_id": session_id });
            if action != "rename" {
                body["method"] = json!(path);
            }
            if let Some(title) = args.get("title").and_then(Value::as_str) {
                body["title"] = json!(title);
            }
            if action == "rename" && body.get("title").is_none() {
                bail!("rename requires a title");
            }
            if action == "delete" && !args.get("confirm").and_then(Value::as_bool).unwrap_or(false) {
                bail!("deleting a session is irreversible; re-run with \"confirm\": true");
            }
            Ok(pretty(client.raw_rpc("POST", path, Some(body)).await?))
        }
        "muse_settings" => {
            let action = args
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("read");
            match action {
                "read" => {
                    let mut out = serde_json::Map::new();
                    for (key, path) in [
                        ("model", "/model"),
                        ("identity", "/api/identity"),
                        ("tab_order", "/api/navigation/tab-order"),
                        ("product_improvements", "/api/product-improvements"),
                        ("voice_active", "/api/voice/active-voice"),
                        ("chat_themes", "/api/chat/themes"),
                        ("onboarding_tasks", "/api/onboarding/tasks"),
                        ("onboarding_variants", "/api/onboarding/variants"),
                    ] {
                        match client.raw_rpc("GET", path, None).await {
                            Ok(value) => {
                                out.insert(key.to_string(), value);
                            }
                            Err(err) => {
                                out.insert(key.to_string(), json!({ "error": err.to_string() }));
                            }
                        }
                    }
                    Ok(pretty(Value::Object(out)))
                }
                "set_tab_order" => {
                    let order = args
                        .get("tab_order")
                        .cloned()
                        .ok_or_else(|| anyhow!("set_tab_order requires `tab_order`"))?;
                    Ok(pretty(
                        client
                            .raw_rpc(
                                "POST",
                                "/api/navigation/tab-order",
                                Some(json!({ "tab_order": order })),
                            )
                            .await?,
                    ))
                }
                "set_voice" => {
                    let voice = args
                        .get("voice_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("set_voice requires `voice_id`"))?;
                    Ok(pretty(
                        client
                            .raw_rpc(
                                "POST",
                                "/api/voice/active-voice",
                                Some(json!({ "voice_id": voice })),
                            )
                            .await?,
                    ))
                }
                other => bail!("unknown settings action {other}; expected read/set_tab_order/set_voice"),
            }
        }
        "muse_threads" => {
            let sessions = client.sessions().await?;
            let rows = crate::thread::parse_sessions(&sessions);
            Ok(pretty(crate::thread::summarise(&rows)))
        }
        "muse_thread" => {
            let action = args
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("new");
            let name = required_str(&args, "name")?;
            crate::thread::validate_name(&name)?;
            match action {
                "new" => {
                    let opening = args.get("prompt").and_then(Value::as_str);
                    let timeout = args
                        .get("timeout_sec")
                        .and_then(Value::as_u64)
                        .unwrap_or(180);
                    let summary = crate::thread::ensure(
                        client,
                        &name,
                        opening,
                        std::time::Duration::from_secs(timeout.max(5)),
                    )
                    .await?;
                    Ok(pretty(json!({
                        "created": true,
                        "thread": name,
                        "session_id": summary.session_id,
                    })))
                }
                other => {
                    let resolved = crate::thread::resolve(client, Some(&name)).await?;
                    if !resolved.exists {
                        return Err(crate::thread::missing_error(&name));
                    }
                    let session_id = resolved.session_id.expect("named thread has an id");
                    let mut body = json!({ "session_id": session_id });
                    let path = match other {
                        "rename" => {
                            let title = args
                                .get("title")
                                .and_then(Value::as_str)
                                .ok_or_else(|| anyhow!("rename requires a title"))?;
                            body["title"] = json!(title);
                            "/api/session/rename"
                        }
                        "pin" => {
                            body["method"] = json!("/api/session/pin");
                            "/api/session/pin"
                        }
                        "unpin" => {
                            body["method"] = json!("/api/session/unpin");
                            "/api/session/unpin"
                        }
                        "archive" => {
                            body["method"] = json!("/api/session/archive");
                            "/api/session/archive"
                        }
                        "unarchive" => {
                            body["method"] = json!("/api/session/unarchive");
                            "/api/session/unarchive"
                        }
                        "delete" => {
                            if !args.get("confirm").and_then(Value::as_bool).unwrap_or(false) {
                                bail!("deleting a thread is irreversible; pass confirm=true");
                            }
                            body["method"] = json!("/api/session/delete");
                            "/api/session/delete"
                        }
                        unknown => bail!(
                            "unknown thread action {unknown}; expected new/rename/pin/unpin/archive/unarchive/delete"
                        ),
                    };
                    Ok(pretty(client.raw_rpc("POST", path, Some(body)).await?))
                }
            }
        }
        "muse_exec" => {
            let command = required_str(&args, "command")?;
            let thread_name = args.get("thread").and_then(Value::as_str);
            let session_id = match thread_name {
                None => None,
                Some(name) => {
                    let resolved = crate::thread::resolve(client, Some(name)).await?;
                    if !resolved.exists {
                        return Err(crate::thread::missing_error(name));
                    }
                    resolved.session_id
                }
            };
            let timeout = args
                .get("timeout_sec")
                .and_then(Value::as_u64)
                .unwrap_or(240);
            let result = crate::sysfs::exec(
                client,
                &command,
                session_id.as_deref(),
                thread_name,
                std::time::Duration::from_secs(timeout.max(10)),
            )
            .await?;
            Ok(pretty(result.to_json()))
        }
        "muse_sys" => {
            let verb_raw = required_str(&args, "verb")?;
            let target = required_str(&args, "target")?;
            let verb = crate::sysfs::Verb::parse(&verb_raw)?;
            let thread_name = args.get("thread").and_then(Value::as_str);
            let session_id = match thread_name {
                None => None,
                Some(name) => {
                    let resolved = crate::thread::resolve(client, Some(name)).await?;
                    if !resolved.exists {
                        return Err(crate::thread::missing_error(name));
                    }
                    resolved.session_id
                }
            };
            let timeout = args
                .get("timeout_sec")
                .and_then(Value::as_u64)
                .unwrap_or(240);
            let result = crate::sysfs::run_verb(
                client,
                verb,
                &target,
                session_id.as_deref(),
                thread_name,
                std::time::Duration::from_secs(timeout.max(10)),
            )
            .await?;
            Ok(pretty(result.to_json()))
        }
        other => Err(anyhow!("unknown tool: {other}")),
    }
}

fn guard_account_path(path: &str) -> Result<()> {
    if !path.starts_with("/hatch/") {
        bail!("account endpoints live under /hatch/; got {path}");
    }
    Ok(())
}

/// Verbs and routes that discard data. The generic RPC tool refuses them unless
/// the caller states which record it means to remove.
fn is_destructive(method: &str, path: &str) -> bool {
    let method = method.to_ascii_uppercase();
    if matches!(method.as_str(), "DELETE" | "PUT") {
        return true;
    }
    method == "POST"
        && (path.contains("/delete")
            || path.contains("/remove")
            || path.contains("/disconnect")
            || path.contains("/unlink")
            || path.contains("/revoke"))
}

fn required_str(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("missing required argument `{key}`"))
}

/// Credentials must never reach the calling model's transcript. Any key whose
/// name looks like a secret is replaced before the payload is serialized.
fn redact(value: Value) -> Value {
    const SECRET_MARKERS: [&str; 8] = [
        "token",
        "secret",
        "password",
        "cookie",
        "csrf",
        "authorization",
        "passkey",
        "credential",
    ];
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (key, value) in map {
                let lowered = key.to_ascii_lowercase();
                if SECRET_MARKERS.iter().any(|marker| lowered.contains(marker))
                    && value.is_string()
                {
                    let rendered = value.as_str().unwrap_or_default();
                    out.insert(
                        key,
                        json!(format!("REDACTED ({} chars)", rendered.chars().count())),
                    );
                } else {
                    out.insert(key, redact(value));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(redact).collect()),
        other => other,
    }
}

fn pretty(value: Value) -> String {
    render_public(&value)
}

/// Serialize a payload for output with every credential-shaped field redacted.
pub fn render_public(value: &Value) -> String {
    let value = redact(value.clone());
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
}
