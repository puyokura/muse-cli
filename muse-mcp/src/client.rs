//! High-level Muse client: session lifecycle, chat, and the workspace
//! filesystem exposed by the Hatch VM.
//!
//! Every RPC here was verified against the shipped clients: the Android app
//! (`HatchHttpApi`, `HatchVmRepository`) and the web bundle both drive the same
//! `/chat/*` and `/fs/*` routes over the Noise channel.

use crate::http::{Bootstrap, MuseHttp};
use crate::transport::HatchConnection;
use crate::wire::Header;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

pub const CHAT_CAPABILITIES: [&str; 4] = [
    "chat_cancel",
    "delta_stream",
    "custom_reactions",
    "custom_reactions_facebook_thumbs_up_v1",
];

const RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const UPLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);
pub const UPLOAD_CHUNK_SIZE: usize = 512 * 1024;

/// Most gateway routes answer `{"ok": true, "result": …}`; a few answer the bare
/// payload. Callers only ever want the payload, so the envelope is removed once
/// here instead of at every call site.
fn unwrap_envelope(value: Value) -> Result<Value> {
    let Some(object) = value.as_object() else {
        return Ok(value);
    };
    if object.get("ok").and_then(Value::as_bool) != Some(true) {
        return Ok(value);
    }
    match object.get("result") {
        Some(result) => Ok(result.clone()),
        None => Ok(value),
    }
}

/// How many dials one RPC may spend before the failure is surfaced.
///
/// One extra dial covers the window between "connect" and the request being
/// written. More than that would spin when the gateway is refusing dials.
pub(crate) const RECONNECT_ATTEMPTS: usize = 2;

/// Whether an error means the socket died rather than the route failing.
fn looks_like_a_dead_socket(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}");
    text.contains("Hatch connection is closed")
        || text.contains("Hatch connection closed")
        || text.contains("Connection reset without closing handshake")
        || text.contains("Hatch stream ended without a response")
        || text.contains("Hatch writer task is not running")
}

/// Whether a cached connection may serve another request.
///
/// A `HatchConnection` outlives its socket. When the reader task records a
/// closure the object stays in the cache but every request on it fails, so
/// reusing it turns one dropped socket into permanent failure for the rest of
/// the process. Anything with a recorded reason must be dialled again.
pub(crate) fn is_reusable(closed_reason: Option<&str>) -> bool {
    closed_reason.is_none()
}

/// Whether an error means "the gateway rejected our credential" rather than
/// "this route failed". Matches the measured shapes (HTTP 401 and the
/// `hatchAdmissionInvalidated` / `session_invalid` codes) without swallowing
/// other 4xx responses.
fn looks_like_auth_failure(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}");
    text.contains("HTTP 401")
        || text.contains("hatchAdmissionInvalidated")
        || text.contains("session_invalid")
        || text.contains("Authentication Error")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(role: &str, message_id: &str, text: &str, status: Option<&str>) -> Value {
        let mut value = json!({
            "event_name": format!("message.{}", role),
            "message_id": message_id,
            "display_text": text,
        });
        if let Some(status) = status {
            value["status"] = json!(status);
        }
        value
    }

    #[test]
    fn finds_the_reply_for_the_submitted_message() {
        let history = json!({
            "chat_events": [
                event("user", "m1", "first", None),
                event("assistant", "a1", "old answer", Some("completed")),
                event("user", "m2", "second", None),
                event("assistant", "a2", "new answer", Some("completed")),
            ]
        });
        let (reply, _) = find_turn_reply(&history, Some("m2")).expect("reply");
        assert_eq!(reply["display_text"], "new answer");
    }

    // Regression: a reply still streaming must not be accepted, otherwise the
    // caller receives a truncated fragment.
    #[test]
    fn ignores_a_streaming_reply() {
        let history = json!({
            "chat_events": [
                event("user", "m1", "prompt", None),
                event("assistant", "a1", "```", Some("streaming")),
            ]
        });
        assert!(find_turn_reply(&history, Some("m1")).is_none());
    }

    // Regression: when the submitted message is older than the history window,
    // the newest assistant event belonged to the previous turn. Returning it
    // produced a stale answer, so the poller must keep waiting instead.
    #[test]
    fn does_not_return_a_previous_turns_reply() {
        let history = json!({
            "chat_events": [
                event("user", "old", "earlier prompt", None),
                event("assistant", "a-old", "earlier answer", Some("completed")),
            ]
        });
        assert!(find_turn_reply(&history, Some("m-new")).is_none());
    }

    #[test]
    fn falls_back_to_newest_reply_without_a_message_id() {
        let history = json!({
            "chat_events": [
                event("assistant", "a1", "one", Some("completed")),
                event("assistant", "a2", "two", Some("completed")),
            ]
        });
        let (reply, _) = find_turn_reply(&history, None).expect("reply");
        assert_eq!(reply["display_text"], "two");
    }

    // Regression: a closed connection used to be cached and handed out again,
    // so every later call in the process failed with "connection is closed"
    // even though a fresh dial would have worked.
    #[test]
    fn a_closed_connection_is_not_reusable() {
        assert!(is_reusable(None), "a live socket can serve another request");
        assert!(
            !is_reusable(Some(
                "WebSocket protocol error: Connection reset without closing handshake"
            )),
            "a socket that died must be dialled again"
        );
        assert!(!is_reusable(Some("any other reason")));
    }

    // A closed handle must be replaced, and the replacement must not loop.
    #[test]
    fn reconnect_is_bounded() {
        assert!(RECONNECT_ATTEMPTS >= 1, "at least one fresh dial is needed");
        assert!(RECONNECT_ATTEMPTS <= 3, "more than three dials is a spin");
    }

    #[test]
    fn dead_socket_detection_matches_measured_shapes() {
        let closed = anyhow::Error::msg(
            "Hatch connection is closed: WebSocket protocol error: Connection reset without closing handshake",
        );
        assert!(looks_like_a_dead_socket(&closed));
        let mid = anyhow::Error::msg("Hatch connection closed during GET /api/identity: done");
        assert!(looks_like_a_dead_socket(&mid));
        let timeout = anyhow::Error::msg("Hatch RPC GET /x timed out after 30s");
        assert!(!looks_like_a_dead_socket(&timeout));
        let not_found = anyhow::Error::msg("Hatch RPC returned HTTP 404: path not found");
        assert!(!looks_like_a_dead_socket(&not_found));
    }

    #[test]
    fn auth_failure_detection_matches_measured_shapes() {
        let unauthorized = anyhow!("Hatch RPC returned HTTP 401: Unauthorized");
        assert!(looks_like_auth_failure(&unauthorized));
        // `anyhow!` treats braces as format specifiers, so build it via Display.
        let invalidated = anyhow::Error::msg(r#"{"type":"hatchAdmissionInvalidated"}"#);
        assert!(looks_like_auth_failure(&invalidated));
        let not_found = anyhow!("Hatch RPC returned HTTP 404: path not found");
        assert!(!looks_like_auth_failure(&not_found));
        let bad_request = anyhow!("HTTP 400: invalid_params");
        assert!(!looks_like_auth_failure(&bad_request));
    }

    #[test]
    fn envelope_unwrap_keeps_plain_payloads() {
        let wrapped = json!({"ok": true, "result": {"sessions": []}});
        assert_eq!(unwrap_envelope(wrapped).expect("unwrap")["sessions"], json!([]));

        let plain = json!({"sessions": []});
        assert_eq!(unwrap_envelope(plain).expect("plain")["sessions"], json!([]));

        let failed = json!({"ok": false, "error": {"code": "x"}});
        assert_eq!(unwrap_envelope(failed).expect("error")["ok"], json!(false));
    }

    #[test]
    fn gateway_path_normalisation_rebases_absolute_paths() {
        assert_eq!(normalize_gateway_path("AGENTS.md"), "AGENTS.md");
        assert_eq!(normalize_gateway_path("/AGENTS.md"), "AGENTS.md");
        // A leading `.` segment stays as-is; the gateway treats it as a name.
        assert_eq!(normalize_gateway_path(r".\x\y"), "./x/y");
        assert_eq!(
            normalize_gateway_path("/workspace/notes/a.md"),
            "workspace/notes/a.md"
        );
        assert_eq!(normalize_gateway_path("home/user/notes/a.md"), "notes/a.md");
        assert_eq!(normalize_gateway_path("file:///x/y.txt"), "x/y.txt");
        assert_eq!(normalize_gateway_path("a.txt?x=1#f"), "a.txt");
        assert_eq!(normalize_gateway_path(""), "");
    }

    #[test]
    fn base64_round_trip() {
        let bytes = b"binary\x00payload";
        let encoded = base64_encode(bytes);
        let decoded = base64_decode(&encoded).expect("decode");
        assert_eq!(decoded, bytes);
        assert!(base64_decode("not base64!").is_err());
    }

}

/// Text of the newest assistant event in a history payload, if any.
fn newest_assistant_text(history: &Value) -> Option<String> {
    let event = newest_assistant_event(history)?;
    event
        .get("display_text")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            event
                .get("payload")
                .and_then(|payload| payload.get("display_text"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

/// Newest assistant event in a history payload.
fn newest_assistant_event(history: &Value) -> Option<Value> {
    let events = history
        .get("chat_events")
        .and_then(Value::as_array)
        .or_else(|| history.get("result")?.get("chat_events")?.as_array())?;
    events
        .iter()
        .rev()
        .find(|event| {
            event
                .get("event_name")
                .and_then(Value::as_str)
                .map(|name| name.starts_with("message.assistant"))
                .unwrap_or(false)
        })
        .cloned()
}

pub struct MuseClient {
    pub http: Mutex<MuseHttp>,
    connection: Mutex<Option<Arc<HatchConnection>>>,
    bootstrap: Mutex<Option<Bootstrap>>,
    device_id: String,
    state_path: PathBuf,
}

/// Result of one prompt round-trip: the gateway acknowledgement plus the
/// assistant reply recovered from history.
pub struct AskOutcome {
    pub session_id: String,
    pub sent_message_id: Option<String>,
    pub reply: Value,
    #[allow(dead_code)]
    pub echoed_input: Value,
    pub events: Vec<Value>,
    pub history: Option<Value>,
}

/// Find the assistant event answering `sent_message_id`.
///
/// History is ordered oldest-first. A streaming reply appears as soon as its
/// first delta lands, carrying `status: "streaming"` and a partial
/// `display_text`; accepting it would return a truncated answer (observed:
/// a bare ` ``` ` fence instead of the command output). Only a reply whose
/// status is terminal counts.
fn find_turn_reply(history: &Value, sent_message_id: Option<&str>) -> Option<(Value, Value)> {
    let events = history
        .get("chat_events")
        .and_then(Value::as_array)
        .or_else(|| history.get("result")?.get("chat_events")?.as_array())?;

    let is_assistant = |event: &Value| {
        event
            .get("event_name")
            .and_then(Value::as_str)
            .map(|name| name.starts_with("message.assistant"))
            .unwrap_or(false)
    };
    let text_of = |event: &Value| {
        event
            .get("display_text")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                event
                    .get("payload")
                    .and_then(|payload| payload.get("display_text"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_default()
    };
    // `status` lives on the event or inside `payload`. Only an explicit terminal
    // value counts: the very first delta of a streaming reply has no `status`
    // yet, and accepting it returns a truncated answer (observed: a bare ```
    // fence instead of the command output).
    let is_settled = |event: &Value| {
        let status = event
            .get("status")
            .and_then(Value::as_str)
            .or_else(|| {
                event
                    .get("payload")
                    .and_then(|payload| payload.get("status"))
                    .and_then(Value::as_str)
            });
        matches!(status, Some("completed") | Some("failed") | Some("error"))
    };
    let is_ready =
        |event: &Value| is_assistant(event) && is_settled(event) && !text_of(event).is_empty();

    if let Some(sent) = sent_message_id {
        let anchor = events.iter().position(|event| {
            event.get("message_id").and_then(Value::as_str) == Some(sent)
                || event.get("reply_to_message_id").and_then(Value::as_str) == Some(sent)
        });
        let Some(index) = anchor else {
            // The submitted message is not in this window yet. Falling back to
            // "the newest assistant reply" here returned the previous turn's
            // answer, so report "not yet" and let the caller keep polling.
            return None;
        };
        let input = events[index].clone();
        for event in events.iter().skip(index + 1) {
            if is_ready(event) {
                return Some((event.clone(), input));
            }
        }
        return None;
    }

    let reply = events.iter().rev().find(|event| is_ready(event))?;
    Some((reply.clone(), Value::Null))
}

impl MuseClient {
    pub fn new(state_path: PathBuf) -> Result<Self> {
        let http = MuseHttp::new(state_path.clone())?;
        let device_id = load_or_create_device_id(&state_path);
        Ok(Self {
            http: Mutex::new(http),
            connection: Mutex::new(None),
            bootstrap: Mutex::new(None),
            device_id,
            state_path,
        })
    }

    pub fn state_path(&self) -> &Path {
        &self.state_path
    }

    pub async fn http_mut(&self) -> tokio::sync::MutexGuard<'_, MuseHttp> {
        self.http.lock().await
    }

    /// Open (or reuse) the Noise connection.
    ///
    /// A cached connection is only reusable while its socket is alive. Once the
    /// reader task records a closure, every request against it fails with
    /// "connection is closed" and never recovers, because nothing reconnects:
    /// the reuse check has to throw the dead handle away and dial again.
    pub async fn connect(&self) -> Result<Arc<HatchConnection>> {
        if let Some(existing) = self.connection.lock().await.clone() {
            if is_reusable(existing.closed_reason().await.as_deref()) {
                return Ok(existing);
            }
            // Dead socket: drop it and fall through to a fresh dial.
            // Resolve the reason before logging: an await inside the macro's
            // argument list keeps its `Arguments` alive across the await, which
            // makes the enclosing future non-Send and unspawnable.
            let reason = existing.closed_reason().await;
            tracing::info!(
                target: "muse_mcp::noise",
                reason = ?reason,
                "dropping a closed Hatch connection and reconnecting"
            );
            self.connection.lock().await.take();
        }
        let mut http = self.http.lock().await;
        let bootstrap = http.bootstrap().await?;
        let bootstrap = http.hatch_token(&bootstrap).await?;
        let candidates = bootstrap.websocket_candidates();
        drop(http);

        // Dial every candidate, then once more after a short pause. A dial can
        // fail transiently (measured: the same shared ingress refused, then
        // accepted five seconds later), and a caller has no way to tell that
        // apart from "the VM is down".
        let mut failures: Vec<String> = Vec::new();
        for round in 0..2 {
            if round > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                failures.clear();
            }
            for url in &candidates {
                match HatchConnection::connect(url, &bootstrap.locale).await {
                    Ok(connection) => {
                        let connection = Arc::new(connection);
                        *self.bootstrap.lock().await = Some(bootstrap);
                        *self.connection.lock().await = Some(connection.clone());
                        return Ok(connection);
                    }
                    Err(err) => failures.push(format!("{err}")),
                }
            }
        }
        bail!("no Hatch gateway accepted the connection: {}", failures.join(" | "))
    }

    pub async fn disconnect(&self) {
        if let Some(connection) = self.connection.lock().await.take() {
            connection.close().await;
        }
    }

    async fn rpc(
        &self,
        method: &str,
        path: &str,
        params: Option<Value>,
    ) -> Result<Value> {
        // A socket can die between "connect" and the request being written.
        // `connect` already replaces a handle it knows is closed, but the
        // closure can land afterwards, so one fresh dial covers the remaining
        // window. The budget stays at one to avoid spinning.
        let mut attempt = 0usize;
        loop {
            attempt += 1;
            match self.rpc_once(method, path, params.clone()).await {
                Ok(value) => return Ok(value),
                Err(err)
                    if looks_like_a_dead_socket(&err) && attempt < RECONNECT_ATTEMPTS =>
                {
                    tracing::info!(
                        target: "muse_mcp::noise",
                        path,
                        attempt,
                        "socket died mid-request; dialling again"
                    );
                    self.disconnect().await;
                }
                Err(err) => return self.recover_and_retry(err, method, path, params).await,
            }
        }
    }

    /// Handle a failure the retry loop gave up on: refresh a credential, or
    /// surface the error unchanged.
    async fn recover_and_retry(
        &self,
        err: anyhow::Error,
        method: &str,
        path: &str,
        params: Option<Value>,
    ) -> Result<Value> {
        if !looks_like_auth_failure(&err) {
            return Err(err);
        }
        // A cached abra token expires while the browser cookies stay valid.
        // Re-running auth_check re-issues it without another OTP, so the caller
        // only sees the retry when that fails too.
        let recovered = {
            let mut http = self.http.lock().await;
            http.auth_check().await.map(|check| {
                check
                    .get("ok")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            })
        };
        match recovered {
            Ok(true) => {
                tracing::info!(
                    target: "muse_mcp::auth",
                    "recovered an expired gateway credential via auth_check"
                );
                self.disconnect().await;
                self.rpc_once(method, path, params).await
            }
            Ok(false) => Err(err
                .context("401 and the stored session no longer validates; run `muse login`")),
            Err(recover_err) => Err(err
                .context(format!("401 and the credential refresh failed: {recover_err}"))),
        }
    }

    /// One attempt with no recovery, so the retry path cannot loop.
    async fn rpc_once(
        &self,
        method: &str,
        path: &str,
        params: Option<Value>,
    ) -> Result<Value> {
        let connection = self.connect().await?;
        let response = connection
            .request(method, path, params, 0, RPC_TIMEOUT)
            .await?;
        let response = response.ensure_ok()?;
        if !response.is_json() {
            bail!(
                "{path} returned {} bytes of binary data; use `muse fs get` or `muse raw`",
                response.body.len()
            );
        }
        let value = response.json()?;
        Ok(unwrap_envelope(value)?)
    }

    /// Raw bytes from any gateway route. Used by `/fs/raw/*`, `/fs/export`, and
    /// media handles whose response is not JSON.
    #[allow(dead_code)]
    pub async fn raw_bytes(
        &self,
        method: &str,
        path: &str,
        params: Option<Value>,
        timeout: std::time::Duration,
    ) -> Result<Vec<u8>> {
        let connection = self.connect().await?;
        let response = connection
            .request(method, path, params, 0, timeout)
            .await?
            .ensure_ok()?;
        Ok(response.body)
    }

    pub async fn ping(&self) -> Result<Value> {
        self.rpc("POST", "/api/ping", Some(json!({}))).await
    }

    pub async fn model(&self) -> Result<Value> {
        self.rpc("GET", "/model", None).await
    }

    pub async fn sessions(&self) -> Result<Value> {
        self.rpc("GET", "/api/session/list", None).await
    }

    /// Archived conversations. They are omitted from the default listing, so
    /// resolving an archived thread needs this separate view.
    pub async fn archived_sessions(&self) -> Result<Value> {
        self.rpc("GET", "/api/session/list?view=archived", None).await
    }

    pub async fn history(&self, session_id: Option<&str>, limit: u32) -> Result<Value> {
        let mut params = json!({ "limit": limit, "transcript_mode": "messages" });
        if let Some(session_id) = session_id {
            params["session_id"] = json!(session_id);
        }
        self.rpc("GET", "/chat/history", Some(params)).await
    }

    /// Escape hatch for Hatch routes that do not yet have a dedicated tool.
    /// Any verb and path reaches the gateway's daemon service unchanged, so new
    /// server routes can be exercised without rebuilding the client.
    pub async fn raw_rpc(
        &self,
        method: &str,
        path: &str,
        params: Option<Value>,
    ) -> Result<Value> {
        if !path.starts_with('/') {
            bail!("Hatch path must start with '/'; got {path}");
        }
        match method.to_ascii_uppercase().as_str() {
            "GET" | "POST" | "PUT" | "PATCH" | "DELETE" => {}
            other => bail!("unsupported method {other}"),
        }
        self.rpc(&method.to_ascii_uppercase(), path, params).await
    }

    /// Send a prompt and collect the streamed events.
    pub async fn ask(
        &self,
        prompt: &str,
        session_id: Option<&str>,
        attachments: &[PathBuf],
        idle_timeout: std::time::Duration,
    ) -> Result<Vec<Value>> {
        self.ask_with_headers(prompt, session_id, attachments, idle_timeout, &[])
            .await
    }

    /// Same as [`Self::ask`] with caller-supplied headers on the wire. Used for
    /// relay turns, where the correlation ids must reach the gateway.
    pub async fn ask_with_headers(
        &self,
        prompt: &str,
        session_id: Option<&str>,
        attachments: &[PathBuf],
        idle_timeout: std::time::Duration,
        extra: &[Header],
    ) -> Result<Vec<Value>> {
        if prompt.trim().is_empty() {
            bail!("prompt must not be empty");
        }
        let connection = self.connect().await?;
        let mut items: Vec<Value> = Vec::new();
        for path in attachments {
            items.push(image_item(path)?);
        }
        items.push(json!({ "type": "text", "text": prompt }));

        let mut payload = json!({
            "items": items,
            "node_id": self.device_id,
            "capabilities": CHAT_CAPABILITIES,
        });
        if let Some(session_id) = session_id {
            payload["session_id"] = json!(session_id);
        }
        connection
            .stream_events("/chat/stream", payload, idle_timeout, 4000, extra)
            .await
    }

    /// Send a prompt, then poll history until the assistant reply for that turn
    /// appears. `/chat/stream` only acknowledges the submission (one event with
    /// the message and session ids), so the reply has to come from history.
    pub async fn ask_and_wait(
        &self,
        prompt: &str,
        session_id: Option<&str>,
        attachments: &[PathBuf],
        reply_timeout: std::time::Duration,
        poll_interval: std::time::Duration,
    ) -> Result<AskOutcome> {
        self.ask_and_wait_with_headers(
            prompt,
            session_id,
            attachments,
            reply_timeout,
            poll_interval,
            &[],
        )
        .await
    }

    /// Same as [`Self::ask_and_wait`] with caller-supplied headers on the wire.
    pub async fn ask_and_wait_with_headers(
        &self,
        prompt: &str,
        session_id: Option<&str>,
        attachments: &[PathBuf],
        reply_timeout: std::time::Duration,
        poll_interval: std::time::Duration,
        extra: &[Header],
    ) -> Result<AskOutcome> {
        let events = self
            .ask_with_headers(prompt, session_id, attachments, reply_timeout, extra)
            .await?;
        // A submission that produced no acknowledgement never reached the
        // gateway. Reporting "no reply" for it would read as "the agent stayed
        // silent", which is a different fact and the wrong one to hand back.
        if events.is_empty() {
            bail!(
                "the gateway returned no acknowledgement for the submission; the prompt was \
                 not confirmed as delivered"
            );
        }
        let ack = events.first().cloned().unwrap_or(Value::Null);
        let session_id = ack
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| session_id.map(str::to_string));
        let sent_message_id = ack
            .get("message_id")
            .and_then(Value::as_str)
            .map(str::to_string);

        let Some(session) = session_id.clone() else {
            bail!(
                "the gateway acknowledged the prompt but returned no session id; \
                 poll muse_history manually. ack={ack}"
            );
        };

        let deadline = std::time::Instant::now() + reply_timeout;
        let mut last_seen: Option<Value> = None;
        // A reply is accepted when its status is terminal. Some routes omit the
        // field entirely, so a second signal is also honoured: the same text
        // seen twice in a row after it has stopped growing.
        let mut previous_text: Option<String> = None;
        let mut stable_polls = 0usize;

        while std::time::Instant::now() < deadline {
            tokio::time::sleep(poll_interval).await;
            let history = match self.history(Some(&session), 40).await {
                Ok(history) => history,
                Err(err) => {
                    last_seen = Some(json!({ "history_error": err.to_string() }));
                    continue;
                }
            };

            if let Some((reply, input)) = find_turn_reply(&history, sent_message_id.as_deref()) {
                return Ok(AskOutcome {
                    session_id: session,
                    sent_message_id,
                    reply,
                    echoed_input: input,
                    events,
                    history: Some(history),
                });
            }

            // Fallback for replies without an explicit terminal status: accept
            // once the newest assistant text is non-empty and repeats unchanged.
            let current = newest_assistant_text(&history);
            match (&previous_text, &current) {
                (Some(before), Some(now)) if before == now && !now.trim().is_empty() => {
                    stable_polls += 1;
                    if stable_polls >= 2 {
                        if let Some(reply) = newest_assistant_event(&history) {
                            return Ok(AskOutcome {
                                session_id: session,
                                sent_message_id,
                                reply,
                                echoed_input: Value::Null,
                                events,
                                history: Some(history),
                            });
                        }
                    }
                }
                _ => stable_polls = 0,
            }
            previous_text = current;
            last_seen = Some(history);
        }
        Ok(AskOutcome {
            session_id: session,
            sent_message_id,
            reply: Value::Null,
            echoed_input: Value::Null,
            events,
            history: last_seen,
        })
    }

    pub async fn cancel_stream(&self, session_id: &str) -> Result<Value> {
        self.rpc(
            "POST",
            "/chat/cancel",
            Some(json!({ "session_id": session_id })),
        )
        .await
    }

    pub async fn fs_list(&self, path: &str) -> Result<Value> {
        self.rpc(
            "POST",
            "/fs/list",
            Some(json!({ "path": normalize_gateway_path(path) })),
        )
        .await
    }

    pub async fn fs_stat(&self, path: &str) -> Result<Value> {
        self.rpc(
            "POST",
            "/fs/stat",
            Some(json!({ "path": normalize_gateway_path(path) })),
        )
        .await
    }

    /// Windowed read. `/fs/reads` takes `{path, len, offset}` and returns the
    /// slice as both base64 and (for UTF-8 content) `text`; the offset was
    /// verified to address mid-file positions.
    pub async fn fs_read_window(
        &self,
        path: &str,
        offset: u64,
        len: usize,
    ) -> Result<(Vec<u8>, Option<String>)> {
        if len == 0 {
            bail!("len must be positive");
        }
        let payload = json!({
            "entries": [{
                "path": normalize_gateway_path(path),
                "len": len,
                "offset": offset,
            }]
        });
        let response = self.rpc("POST", "/fs/reads", Some(payload)).await?;
        let entry = response
            .get("entries")
            .and_then(Value::as_array)
            .and_then(|entries| entries.first())
            .cloned()
            .ok_or_else(|| anyhow!("fs/reads returned no entries for {path}"))?;
        if let Some(error) = entry.get("error").and_then(Value::as_str) {
            bail!("fs/reads failed for {path}: {error}");
        }
        let encoded = entry
            .get("data_base64")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("fs/reads returned no data for {path}"))?;
        let bytes = base64_decode(encoded)?;
        let text = entry
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok((bytes, text))
    }

    /// Content search. `/fs/search` accepts `name.contains` and/or `extensions`
    /// and answers with `entries`.
    pub async fn fs_search(&self, query: &str, extensions: &[String]) -> Result<Value> {
        let mut payload = serde_json::Map::new();
        if !query.trim().is_empty() {
            payload.insert(
                "name".to_string(),
                json!({ "contains": query.trim() }),
            );
        }
        if !extensions.is_empty() {
            payload.insert("extensions".to_string(), json!(extensions));
        }
        if payload.is_empty() {
            bail!("search needs a query or at least one extension");
        }
        self.rpc("POST", "/fs/search", Some(Value::Object(payload)))
            .await
    }

    /// `/fs/library` with `preset` in {media, artifacts, recents}.
    pub async fn fs_library(&self, preset: &str) -> Result<Value> {
        match preset {
            "media" | "artifacts" | "recents" => {}
            other => bail!("unknown library preset `{other}` (media, artifacts, recents)"),
        }
        self.rpc("POST", "/fs/library", Some(json!({ "preset": preset })))
            .await
    }

    /// Create a directory. `/fs/mkdir` requires `recursive`.
    pub async fn fs_mkdir(&self, path: &str, recursive: bool) -> Result<Value> {
        self.rpc(
            "POST",
            "/fs/mkdir",
            Some(json!({
                "path": normalize_gateway_path(path),
                "recursive": recursive,
            })),
        )
        .await
    }

    /// Raw bytes for a path through `/fs/raw` (binary-safe).
    pub async fn fs_download(&self, path: &str) -> Result<Vec<u8>> {
        let normalized = normalize_gateway_path(path);
        let route = format!("/fs/raw/{}", urlencode_path(&normalized));
        self.raw_bytes("GET", &route, None, std::time::Duration::from_secs(600))
            .await
    }

    pub async fn fs_read(&self, path: &str, max_bytes: usize) -> Result<(Vec<u8>, bool)> {
        let normalized = normalize_gateway_path(path);
        if max_bytes == 0 {
            bail!("max_bytes must be positive");
        }
        let route = format!("/fs/raw/{}", urlencode_path(&normalized));
        let connection = self.connect().await?;
        let response = connection
            .request(
                "GET",
                &route,
                None,
                0,
                std::time::Duration::from_secs(300),
            )
            .await?
            .ensure_ok()?;
        let truncated = response.body.len() > max_bytes;
        let body = if truncated {
            response.body[..max_bytes].to_vec()
        } else {
            response.body
        };
        Ok((body, truncated))
    }

    pub async fn fs_write(&self, path: &str, data: &[u8], append: bool) -> Result<Value> {
        let normalized = normalize_gateway_path(path);
        let encoded = base64_encode(data);
        self.rpc(
            "POST",
            "/fs/write",
            Some(json!({
                "path": normalized,
                "data_base64": encoded,
                "overwrite": !append,
                "append": append,
                "create_parent": true,
            })),
        )
        .await
    }

    pub async fn fs_delete(&self, path: &str, recursive: bool) -> Result<Value> {
        self.rpc(
            "POST",
            "/fs/delete",
            Some(json!({
                "path": normalize_gateway_path(path),
                "recursive": recursive,
            })),
        )
        .await
    }

    pub async fn fs_rename(&self, from: &str, to: &str) -> Result<Value> {
        self.rpc(
            "POST",
            "/fs/rename",
            Some(json!({
                "source": normalize_gateway_path(from),
                "destination": normalize_gateway_path(to),
                "create_parent": true,
                "overwrite": true,
            })),
        )
        .await
    }

    /// Upload a local file in 512 KiB chunks, staging multi-chunk writes so a
    /// partial transfer never replaces the destination.
    pub async fn fs_upload(&self, local: &Path, remote: &str) -> Result<Value> {
        let metadata = std::fs::metadata(local)
            .map_err(|err| anyhow!("cannot read {}: {err}", local.display()))?;
        if !metadata.is_file() {
            bail!("{} is not a regular file", local.display());
        }
        let size = metadata.len() as usize;
        let destination = normalize_gateway_path(remote);
        let total_chunks = if size == 0 {
            0
        } else {
            (size + UPLOAD_CHUNK_SIZE - 1) / UPLOAD_CHUNK_SIZE
        };
        let staged = total_chunks > 1;
        let temp = if staged {
            let parent = destination
                .rsplit_once('/')
                .map(|(parent, _)| parent.to_string())
                .unwrap_or_default();
            let name = format!(".upload-{}.part", uuid::Uuid::new_v4());
            if parent.is_empty() {
                name
            } else {
                format!("{parent}/{name}")
            }
        } else {
            destination.clone()
        };

        let result = self.upload_chunks(local, &temp, size, total_chunks).await;
        match result {
            Ok(()) => {
                if staged {
                    let info = self.fs_stat(&temp).await?;
                    let uploaded = info.get("size").and_then(Value::as_u64).unwrap_or_default();
                    if uploaded != size as u64 {
                        let _ = self.fs_delete(&temp, false).await;
                        bail!(
                            "upload size mismatch: server reports {uploaded} bytes, local file is {size}"
                        );
                    }
                    self.fs_rename(&temp, &destination).await?;
                }
                Ok(json!({
                    "path": destination,
                    "size": size,
                    "chunks": total_chunks,
                    "staged": staged,
                }))
            }
            Err(err) => {
                if staged {
                    let _ = self.fs_delete(&temp, false).await;
                }
                Err(err)
            }
        }
    }

    async fn upload_chunks(
        &self,
        local: &Path,
        target: &str,
        size: usize,
        total_chunks: usize,
    ) -> Result<()> {
        use std::io::Read;
        let mut handle = std::fs::File::open(local)?;
        if size == 0 {
            self.write_chunk(target, b"", false).await?;
            return Ok(());
        }
        let mut buffer = vec![0u8; UPLOAD_CHUNK_SIZE];
        for index in 0..total_chunks {
            let read = handle.read(&mut buffer)?;
            if read == 0 {
                bail!("local file ended after {} of {total_chunks} chunks", index);
            }
            self.write_chunk(target, &buffer[..read], index != 0).await?;
        }
        Ok(())
    }

    async fn write_chunk(&self, path: &str, data: &[u8], append: bool) -> Result<()> {
        let connection = self.connect().await?;
        let payload = json!({
            "path": normalize_gateway_path(path),
            "data_base64": base64_encode(data),
            "overwrite": !append,
            "append": append,
            "create_parent": true,
        });
        connection
            .request("POST", "/fs/write", Some(payload), 0, UPLOAD_TIMEOUT)
            .await?
            .ensure_ok()?;
        Ok(())
    }
}

fn load_or_create_device_id(state_path: &Path) -> String {
    let marker = state_path.with_file_name("device.json");
    if let Ok(raw) = std::fs::read_to_string(&marker) {
        if let Ok(value) = serde_json::from_str::<Value>(&raw) {
            if let Some(id) = value.get("device_id").and_then(Value::as_str) {
                return id.to_string();
            }
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    if let Some(parent) = marker.parent() {
        // The state file itself is 0600; the directory that holds it must not be
        // world-traversable either, or the file mode is the only remaining
        // barrier. 0700 is also what `$XDG_CONFIG_HOME` users expect.
        let _ = std::fs::create_dir_all(parent);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(parent) {
                let mut perms = meta.permissions();
                perms.set_mode(0o700);
                let _ = std::fs::set_permissions(parent, perms);
            }
        }
    }
    let _ = std::fs::write(&marker, serde_json::to_string_pretty(&json!({ "device_id": id })).unwrap_or_default());
    id
}

/// Gateway paths are workspace-relative: strip query/fragment, unify
/// separators, and rebase anything that points outside `workspace/`.
pub fn normalize_gateway_path(value: &str) -> String {
    let mut text = value.trim().to_string();
    if let Some((head, _)) = text.split_once('?') {
        text = head.to_string();
    }
    if let Some((head, _)) = text.split_once('#') {
        text = head.to_string();
    }
    if let Some(stripped) = text.strip_prefix("file://") {
        text = stripped.to_string();
    }
    text = text.replace('\\', "/");
    while text.contains("//") {
        text = text.replace("//", "/");
    }
    text = text.trim_start_matches('/').to_string();
    let parts: Vec<&str> = text.split('/').filter(|part| !part.is_empty()).collect();
    if parts.is_empty() {
        return String::new();
    }
    let rebased: Vec<&str> = if let Some(index) = parts.iter().position(|part| *part == "workspace") {
        parts[index..].to_vec()
    } else if parts.len() >= 3 && (parts[0] == "home" || parts[0] == "Users") {
        parts[2..].to_vec()
    } else {
        parts
    };
    rebased.join("/")
}

fn urlencode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

pub fn base64_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}


fn image_item(path: &Path) -> Result<Value> {
    let mime = match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        other => bail!("unsupported attachment type .{other} (expected png/jpeg/webp/gif)"),
    };
    let raw = std::fs::read(path)
        .map_err(|err| anyhow!("cannot read attachment {}: {err}", path.display()))?;
    const LIMIT: usize = 3_500_000;
    if raw.len() > LIMIT {
        bail!(
            "attachment {} is {} bytes; the gateway's inline base64 budget is {LIMIT} bytes. \
             Resize it first.",
            path.display(),
            raw.len()
        );
    }
    Ok(json!({
        "type": "image",
        "mime_type": mime,
        "data_base64": base64_encode(&raw),
        "filename": path.file_name().and_then(|name| name.to_str()).unwrap_or("image"),
    }))
}

pub fn base64_decode(text: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|err| anyhow!("invalid base64 payload: {err}"))
}
