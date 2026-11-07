//! Noise-over-WebSocket transport with multiplexed request streams.
//!
//! One WebSocket carries every RPC. Each request allocates a `stream_id`; the
//! reader task decrypts frames and routes them to the owning stream's channel.

use crate::noise::CipherState;
use crate::wire::{
    decode_service_frame, decode_service_response, encode_service_frame_request,
    encode_service_request, split_noise_payload, ApplicationRequest, ApplicationResponse, FramePayload,
    Header, Reassembler, SERVICE_DAEMON,
};
use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;

pub struct RpcResponse {
    pub status: u32,
    #[allow(dead_code)]
    pub headers: Vec<Header>,
    pub body: Vec<u8>,
}

impl RpcResponse {
    pub fn json(&self) -> Result<serde_json::Value> {
        if self.body.is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_slice(&self.body).with_context(|| {
            format!(
                "Hatch RPC returned a non-JSON body ({} bytes)",
                self.body.len()
            )
        })
    }

    /// Binary payloads (`/fs/raw/*`, `/fs/export`, media handles) are not JSON.
    /// Callers that can accept bytes use this instead of `json`.
    pub fn is_json(&self) -> bool {
        let head = self
            .body
            .iter()
            .find(|byte| !byte.is_ascii_whitespace())
            .copied();
        matches!(head, Some(b'{') | Some(b'[') | Some(b'"') | Some(b't') | Some(b'f'))
            || self.body.is_empty()
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn ensure_ok(self) -> Result<Self> {
        if self.status >= 400 {
            bail!(
                "Hatch RPC returned HTTP {}: {}",
                self.status,
                truncate(&self.text(), 600)
            );
        }
        Ok(self)
    }
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push_str("…(truncated)");
    out
}

enum Incoming {
    Frame(crate::wire::ServiceFrame),
    Closed(Option<String>),
}

struct Transport {
    tx: CipherState,
    rx: CipherState,
    reassembler: Reassembler,
}

impl Transport {
    fn seal(&mut self, data: &[u8]) -> Result<Vec<Vec<u8>>> {
        let chunks = split_noise_payload(data).map_err(anyhow::Error::msg)?;
        Ok(chunks
            .into_iter()
            .map(|chunk| self.tx.seal(b"", &chunk))
            .collect())
    }

    fn open(&mut self, data: &[u8]) -> Result<Option<crate::wire::ServiceFrame>> {
        let plain = self.rx.open(b"", data).map_err(anyhow::Error::msg)?;
        let Some(assembled) = self.reassembler.feed(&plain).map_err(anyhow::Error::msg)? else {
            return Ok(None);
        };
        let service_payload = decode_service_response(&assembled).map_err(anyhow::Error::msg)?;
        let frame = decode_service_frame(&service_payload).map_err(anyhow::Error::msg)?;
        Ok(Some(frame))
    }
}

pub struct HatchConnection {
    locator: String,
    tx_frames: mpsc::UnboundedSender<Vec<u8>>,
    streams: Arc<Mutex<HashMap<i64, mpsc::UnboundedSender<Incoming>>>>,
    next_stream_id: AtomicI64,
    locale: String,
    closed: Arc<Mutex<Option<String>>>,
}

impl HatchConnection {
    pub async fn connect(ws_url: &str, locale: &str) -> Result<Self> {
        let locator = redact_url(ws_url);
        let (stream, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .with_context(|| format!("Hatch WebSocket connect failed for {locator}"))?;
        let (mut sink, mut source) = stream.split();

        let mut initiator = crate::noise::Initiator::new();
        let message1 = initiator.write_message1();
        sink.send(Message::Binary(message1)).await?;

        let message2 = tokio::time::timeout(std::time::Duration::from_secs(20), source.next())
            .await
            .map_err(|_| anyhow!("Noise handshake timed out waiting for the responder message"))?
            .ok_or_else(|| anyhow!("Hatch closed the socket during the Noise handshake"))??;
        let bytes = match message2 {
            Message::Binary(data) => data,
            Message::Close(frame) => bail!(
                "Hatch closed the socket during the Noise handshake: {}",
                frame.map(|f| format!("{} {}", f.code, f.reason)).unwrap_or_default()
            ),
            other => bail!("expected a binary Noise handshake message, got {other:?}"),
        };
        let attestation = initiator
            .read_message2(&bytes)
            .map_err(|err| anyhow!(err))
            .context("Noise message2 decryption failed")?;
        let message3 = initiator.write_message3().map_err(|err| anyhow!(err))?;
        sink.send(Message::Binary(message3)).await?;
        let (tx, rx) = initiator.split();

        let transport = Arc::new(Mutex::new(Transport {
            tx,
            rx,
            reassembler: Reassembler::new(),
        }));
        let streams: Arc<Mutex<HashMap<i64, mpsc::UnboundedSender<Incoming>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(Mutex::new(None::<String>));
        let (frame_tx, mut frame_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        // Reader: decrypt and fan out.
        {
            let transport = transport.clone();
            let streams = streams.clone();
            let closed = closed.clone();
            tokio::spawn(async move {
                loop {
                    let message = match source.next().await {
                        Some(Ok(message)) => message,
                        Some(Err(err)) => {
                            *closed.lock().await = Some(err.to_string());
                            break;
                        }
                        None => break,
                    };
                    let data = match message {
                        Message::Binary(data) => data,
                        Message::Close(_) => break,
                        _ => continue,
                    };
                    let frame = {
                        let mut transport = transport.lock().await;
                        match transport.open(&data) {
                            Ok(frame) => frame,
                            Err(err) => {
                                *closed.lock().await = Some(err.to_string());
                                break;
                            }
                        }
                    };
                    let Some(frame) = frame else { continue };
                    let sender = streams.lock().await.get(&frame.stream_id).cloned();
                    if let Some(sender) = sender {
                        let _ = sender.send(Incoming::Frame(frame));
                    }
                }
                let reason = closed.lock().await.clone();
                for (_, sender) in streams.lock().await.drain() {
                    let _ = sender.send(Incoming::Closed(reason.clone()));
                }
            });
        }

        // Writer: seal and send.
        {
            let transport = transport.clone();
            tokio::spawn(async move {
                while let Some(payload) = frame_rx.recv().await {
                    let sealed = {
                        let mut transport = transport.lock().await;
                        transport.seal(&payload)
                    };
                    match sealed {
                        Ok(frames) => {
                            for frame in frames {
                                if sink.send(Message::Binary(frame)).await.is_err() {
                                    return;
                                }
                            }
                        }
                        Err(_) => return,
                    }
                }
            });
        }

        tracing::debug!(
            target: "muse_mcp::noise",
            attestation_bytes = attestation.len(),
            "Noise XX handshake completed"
        );

        Ok(Self {
            locator,
            tx_frames: frame_tx,
            streams,
            next_stream_id: AtomicI64::new(1),
            locale: locale.to_string(),
            closed,
        })
    }

    fn headers(&self, json_body: bool) -> Vec<Header> {
        let mut headers = Vec::new();
        if json_body {
            headers.push(Header::new("Content-Type", "application/json"));
        }
        headers.push(Header::new("x-request-id", &uuid::Uuid::new_v4().to_string()));
        headers.push(Header::new("x-app-id", "hatch-web"));
        headers.push(Header::new("Accept-Language", &self.locale));
        headers
    }

    async fn open_stream(
        &self,
        method: &str,
        path: &str,
        body: Vec<u8>,
        json_body: bool,
        service: u64,
    ) -> Result<(i64, mpsc::UnboundedReceiver<Incoming>)> {
        if let Some(reason) = self.closed.lock().await.clone() {
            bail!("Hatch connection is closed: {reason}");
        }
        let stream_id = self.next_stream_id.fetch_add(1, Ordering::SeqCst);
        let request = ApplicationRequest {
            verb: method.to_string(),
            path: path.to_string(),
            headers: self.headers(json_body),
            body,
            end_body: true,
        };
        let service_frame = encode_service_frame_request(stream_id, &request);
        let envelope = encode_service_request(service, &service_frame);

        let (sender, receiver) = mpsc::unbounded_channel();
        self.streams.lock().await.insert(stream_id, sender);
        if self.tx_frames.send(envelope).is_err() {
            self.streams.lock().await.remove(&stream_id);
            bail!("Hatch writer task is not running ({})", self.locator);
        }
        Ok((stream_id, receiver))
    }

    pub async fn request(
        &self,
        method: &str,
        path: &str,
        params: Option<serde_json::Value>,
        service: u64,
        timeout: std::time::Duration,
    ) -> Result<RpcResponse> {
        let (path, body, json_body) = encode_request(method, path, params)?;
        let (stream_id, mut receiver) =
            self.open_stream(method, &path, body, json_body, service).await?;

        let result = tokio::time::timeout(timeout, async {
            let mut status = 0u32;
            let mut headers = Vec::new();
            let mut chunks: Vec<u8> = Vec::new();
            loop {
                let incoming = receiver
                    .recv()
                    .await
                    .ok_or_else(|| anyhow!("Hatch stream {stream_id} ended without a response"))?;
                match incoming {
                    Incoming::Closed(reason) => {
                        bail!(
                            "Hatch connection closed during {method} {path}: {}",
                            reason.unwrap_or_else(|| "unknown".to_string())
                        )
                    }
                    Incoming::Frame(frame) => match frame.payload {
                        FramePayload::Reset { code, reason } => {
                            bail!("Hatch stream reset (code {code}): {reason}")
                        }
                        FramePayload::Response(response) => {
                            let ApplicationResponse {
                                status: chunk_status,
                                headers: chunk_headers,
                                body,
                                end_body,
                            } = response;
                            status = chunk_status;
                            headers = chunk_headers;
                            chunks.extend_from_slice(&body);
                            if end_body {
                                break;
                            }
                        }
                        FramePayload::BodyChunk { data, end_body } => {
                            chunks.extend_from_slice(&data);
                            if end_body {
                                break;
                            }
                        }
                    },
                }
            }
            Ok::<RpcResponse, anyhow::Error>(RpcResponse {
                status,
                headers,
                body: chunks,
            })
        })
        .await;

        self.streams.lock().await.remove(&stream_id);
        match result {
            Ok(inner) => inner,
            Err(_) => bail!(
                "Hatch RPC {method} {path} timed out after {:.0}s",
                timeout.as_secs_f64()
            ),
        }
    }



    /// POST a streaming endpoint and collect newline-delimited JSON events until
    /// the deadline or `max_events`.
    pub async fn stream_events(
        &self,
        path: &str,
        params: serde_json::Value,
        idle_timeout: std::time::Duration,
        max_events: usize,
    ) -> Result<Vec<serde_json::Value>> {
        let (path, body, json_body) = encode_request("POST", path, Some(params))?;
        let (stream_id, mut receiver) =
            self.open_stream("POST", &path, body, json_body, SERVICE_DAEMON).await?;
        let mut buffer: Vec<u8> = Vec::new();
        let mut events = Vec::new();

        loop {
            let incoming = match tokio::time::timeout(idle_timeout, receiver.recv()).await {
                Ok(Some(incoming)) => incoming,
                Ok(None) => break,
                Err(_) => break,
            };
            match incoming {
                Incoming::Closed(reason) => {
                    if events.is_empty() {
                        self.streams.lock().await.remove(&stream_id);
                        bail!(
                            "Hatch stream closed before any event: {}",
                            reason.unwrap_or_else(|| "unknown".to_string())
                        );
                    }
                    break;
                }
                Incoming::Frame(frame) => {
                    let ended;
                    match frame.payload {
                        FramePayload::Reset { code, reason } => {
                            self.streams.lock().await.remove(&stream_id);
                            bail!("Hatch stream reset (code {code}): {reason}")
                        }
                        FramePayload::Response(response) => {
                            if response.status >= 400 {
                                self.streams.lock().await.remove(&stream_id);
                                bail!(
                                    "Hatch stream returned HTTP {}: {}",
                                    response.status,
                                    truncate(&String::from_utf8_lossy(&response.body), 400)
                                );
                            }
                            buffer.extend_from_slice(&response.body);
                            ended = response.end_body;
                        }
                        FramePayload::BodyChunk { data, end_body } => {
                            buffer.extend_from_slice(&data);
                            ended = end_body;
                        }
                    }
                    while let Some(index) = buffer.iter().position(|b| *b == b'\n') {
                        let line: Vec<u8> = buffer.drain(..=index).collect();
                        let text = String::from_utf8_lossy(&line);
                        if let Ok(value) = serde_json::from_str::<serde_json::Value>(text.trim()) {
                            events.push(value);
                        }
                    }
                    if ended || events.len() >= max_events {
                        break;
                    }
                }
            }
        }

        if !buffer.is_empty() {
            let text = String::from_utf8_lossy(&buffer);
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(text.trim()) {
                events.push(value);
            }
        }
        self.streams.lock().await.remove(&stream_id);
        Ok(events)
    }

    pub async fn close(&self) {
        self.streams.lock().await.clear();
    }

}

fn encode_request(
    method: &str,
    path: &str,
    params: Option<serde_json::Value>,
) -> Result<(String, Vec<u8>, bool)> {
    match method.to_ascii_uppercase().as_str() {
        "GET" => {
            let Some(serde_json::Value::Object(map)) = params else {
                return Ok((path.to_string(), Vec::new(), false));
            };
            if map.is_empty() {
                return Ok((path.to_string(), Vec::new(), false));
            }
            let pairs: Vec<String> = map
                .iter()
                .map(|(key, value)| {
                    let rendered = match value {
                        serde_json::Value::String(text) => text.clone(),
                        other => other.to_string(),
                    };
                    format!("{}={}", urlencode(key), urlencode(&rendered))
                })
                .collect();
            let separator = if path.contains('?') { '&' } else { '?' };
            Ok((format!("{path}{separator}{}", pairs.join("&")), Vec::new(), false))
        }
        _ => {
            let value = params.unwrap_or_else(|| serde_json::json!({}));
            Ok((path.to_string(), serde_json::to_vec(&value)?, true))
        }
    }
}

fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Never log the admission credential: `auth_token` and `notary_token` are
/// stripped from the locator before it reaches tracing or error text.
fn redact_url(url: &str) -> String {
    let Ok(mut parsed) = reqwest::Url::parse(url) else {
        return "<unparsable websocket url>".to_string();
    };
    let keys: Vec<String> = parsed.query_pairs().map(|(k, _)| k.to_string()).collect();
    if keys.iter().any(|k| k == "auth_token" || k == "notary_token") {
        let host = parsed.host_str().unwrap_or("unknown").to_string();
        let vm_id = parsed
            .query_pairs()
            .find(|(k, _)| k == "vm_id")
            .map(|(_, v)| v.to_string())
            .unwrap_or_default();
        return format!("wss://{host}/v1/noise?vm_id={vm_id}&auth_token=REDACTED");
    }
    parsed.set_query(None);
    parsed.to_string()
}
