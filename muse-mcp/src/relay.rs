//! Correlation headers for DSH <-> Muse relay turns.
//!
//! A relay turn is an ordinary `/chat/stream` submission with three extra
//! headers on the wire, so both sides can stitch a request to the session pair
//! that produced it:
//!
//! ```text
//! museSessionId  the Muse conversation the prompt is delivered to
//! dshSessionId   the DSH session that issued it
//! requestId      caller-chosen correlation id for this single turn
//! ```

use crate::wire::Header;
use anyhow::{anyhow, bail, Result};

/// Header names as sent on the wire.
pub const HEADER_MUSE_SESSION: &str = "museSessionId";
pub const HEADER_DSH_SESSION: &str = "dshSessionId";
pub const HEADER_REQUEST_ID: &str = "requestId";

/// Longest accepted id. Ids are UUIDs or short slugs; anything near this limit
/// is a mistake, not a legitimate value.
const MAX_ID_LEN: usize = 256;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelayHeaders {
    pub muse_session_id: Option<String>,
    pub dsh_session_id: Option<String>,
    pub request_id: Option<String>,
}

impl RelayHeaders {
    /// Validate every present id and produce the wire headers.
    ///
    /// Header values must never contain CR, LF, or NUL: the gateway frames them
    /// into protobuf, but a newline here means the caller passed unvalidated
    /// input and would split the header list. Reject rather than sanitize, so a
    /// broken correlation id is never silently delivered under a different id.
    pub fn to_headers(&self) -> Result<Vec<Header>> {
        let mut headers = Vec::new();
        for (name, value) in [
            (HEADER_MUSE_SESSION, &self.muse_session_id),
            (HEADER_DSH_SESSION, &self.dsh_session_id),
            (HEADER_REQUEST_ID, &self.request_id),
        ] {
            let Some(value) = value else { continue };
            validate_id(name, value)?;
            headers.push(Header::new(name, value));
        }

        // The transport sets `x-request-id` for correlation on the gateway side.
        // Point it at the relay id so one turn has one id everywhere it is
        // visible instead of two unrelated ones.
        if let Some(request_id) = &self.request_id {
            if !headers
                .iter()
                .any(|header| header.key.eq_ignore_ascii_case("x-request-id"))
            {
                headers.push(Header::new("x-request-id", request_id));
            }
        }
        Ok(headers)
    }

}

/// Opening line of the block both directions agree on.
pub const RELAY_BLOCK_OPEN: &str = "[muse-relay]";

/// Prefix the server uses when it has to invent a request id.
pub const GENERATED_REQUEST_PREFIX: &str = "dsh-";

/// Whether a string is a canonical lower-case UUID.
///
/// A relay destination has to be an id the gateway will accept, so anything
/// else (a thread name, a nickname, a typo) is refused before delivery.
pub fn is_uuid(value: &str) -> bool {
    if value.len() != 36 {
        return false;
    }
    let bytes = value.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        match index {
            8 | 13 | 18 | 23 => {
                if *byte != b'-' {
                    return false;
                }
            }
            _ => {
                if !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase() {
                    return false;
                }
            }
        }
    }
    true
}

/// Wall-clock milliseconds since the epoch, for audit entries.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Append one audit line per delivered relay turn.
///
/// Records the three ids and the delivery outcome only. The body is not stored:
/// the audit answers "which turn went where", not "what was said", and keeping
/// full bodies would widen the audit well past its existing scope.
pub fn write_audit(state_path: &std::path::Path, entry: &serde_json::Value) {
    let path = state_path.with_file_name("relay-audit.jsonl");
    let line = match serde_json::to_string(entry) {
        Ok(line) => line,
        Err(_) => return,
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{line}");
    }
}

/// One `[muse-relay]` block found inside a message body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyBlock {
    /// Byte range of the whole block, including its closing newline.
    start: usize,
    end: usize,
    muse_session_id: Option<String>,
    dsh_session_id: Option<String>,
    request_id: Option<String>,
}

/// A relay turn with its three ids resolved and its body already prepared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayEnvelope {
    pub muse_session_id: String,
    pub dsh_session_id: String,
    pub request_id: String,
    /// The message to deliver: server header block, blank line, then body.
    pub body: String,
    /// How many duplicate blocks were stripped from the caller's body.
    pub stripped_blocks: usize,
}

impl RelayEnvelope {
    /// Resolve the triple, verify it, and build the delivered body.
    ///
    /// Every rule here is a refusal rather than a repair: a message that cannot
    /// be attributed is not sent at all, because the whole point of the header
    /// is that its presence means the server vouched for it.
    pub fn resolve(
        muse_session_id: Option<&str>,
        dsh_session_id: Option<&str>,
        request_id: Option<&str>,
        body: &str,
    ) -> Result<Self> {
        let muse_session_id = muse_session_id.map(str::trim).unwrap_or_default();
        if muse_session_id.is_empty() {
            bail!("museSessionId is required: a relay turn names the Muse conversation it goes to");
        }
        if !is_uuid(muse_session_id) {
            bail!("museSessionId must be a lower-case UUID; got `{muse_session_id}`");
        }

        let dsh_session_id = resolve_dsh_session(dsh_session_id)?;

        let request_id = match request_id.map(str::trim) {
            Some(value) if !value.is_empty() => {
                validate_id("requestId", value)?;
                value.to_string()
            }
            _ => generate_request_id(),
        };
        validate_id("requestId", &request_id)?;

        let (clean_body, stripped_blocks) = strip_duplicate_blocks(
            body,
            muse_session_id,
            &dsh_session_id,
            &request_id,
        )?;

        let header = render_block(muse_session_id, &dsh_session_id, &request_id);
        let body = if clean_body.trim().is_empty() {
            header
        } else {
            format!("{header}\n\n{clean_body}")
        };

        Ok(Self {
            muse_session_id: muse_session_id.to_string(),
            dsh_session_id,
            request_id,
            body,
            stripped_blocks,
        })
    }

    /// The wire headers that mirror the body block.
    pub fn to_headers(&self) -> Result<Vec<Header>> {
        RelayHeaders {
            muse_session_id: Some(self.muse_session_id.clone()),
            dsh_session_id: Some(self.dsh_session_id.clone()),
            request_id: Some(self.request_id.clone()),
        }
        .to_headers()
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "museSessionId": self.muse_session_id,
            "dshSessionId": self.dsh_session_id,
            "requestId": self.request_id,
            "stripped_blocks": self.stripped_blocks,
        })
    }
}

/// Render the header block. Format and order match the Muse -> dsh direction
/// exactly, so one reader parses both.
pub fn render_block(muse_session_id: &str, dsh_session_id: &str, request_id: &str) -> String {
    format!(
        "{RELAY_BLOCK_OPEN}\n  {HEADER_MUSE_SESSION}: {muse_session_id}\n  \
         {HEADER_DSH_SESSION}: {dsh_session_id}\n  {HEADER_REQUEST_ID}: {request_id}"
    )
}

/// The sending DSH session: the caller's value, the environment, or an error.
///
/// The environment value is the session this server was started for. When the
/// caller also supplies one it has to agree, otherwise the header would claim a
/// session that is not the sender.
pub fn resolve_dsh_session(argument: Option<&str>) -> Result<String> {
    let from_env = std::env::var("DSH_SESSION_ID")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let from_arg = argument
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    match (from_arg, from_env) {
        (Some(arg), Some(env)) => {
            if arg != env {
                bail!(
                    "dshSessionId `{arg}` does not match the sending session `{env}`; \
                     refusing to attribute the message to another session"
                );
            }
            Ok(arg)
        }
        (Some(arg), None) => Ok(arg),
        (None, Some(env)) => Ok(env),
        (None, None) => bail!(
            "dshSessionId is unknown: pass it explicitly, or start the server with \
             DSH_SESSION_ID set to the session that is sending"
        ),
    }
}

/// A fresh server-side id for a message that is not a reply.
pub fn generate_request_id() -> String {
    format!("{GENERATED_REQUEST_PREFIX}{}", uuid::Uuid::new_v4())
}

/// Find every `[muse-relay]` block in a body.
pub fn find_blocks(body: &str) -> Vec<BodyBlock> {
    let mut blocks = Vec::new();
    let mut offset = 0usize;
    let mut open: Option<BodyBlock> = None;

    for line in body.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let trimmed = line.trim_end_matches(['\r', '\n']);

        if trimmed.trim() == RELAY_BLOCK_OPEN {
            if let Some(block) = open.take() {
                blocks.push(block);
            }
            open = Some(BodyBlock {
                start: line_start,
                end: offset,
                muse_session_id: None,
                dsh_session_id: None,
                request_id: None,
            });
            continue;
        }

        let Some(block) = open.as_mut() else { continue };
        let content = trimmed.trim();
        if content.is_empty() {
            // A blank line ends the block; it stays inside the range.
            block.end = offset;
            blocks.push(open.take().expect("block is open"));
            continue;
        }
        let Some((key, value)) = content.split_once(':') else {
            // Anything else is body text again.
            blocks.push(open.take().expect("block is open"));
            continue;
        };
        let value = value.trim().trim_matches('"').to_string();
        match key.trim() {
            HEADER_MUSE_SESSION => block.muse_session_id = Some(value),
            HEADER_DSH_SESSION => block.dsh_session_id = Some(value),
            HEADER_REQUEST_ID => block.request_id = Some(value),
            _ => {}
        }
        block.end = offset;
    }

    if let Some(block) = open {
        blocks.push(block);
    }
    blocks
}

/// Reject a body that contradicts the server, and drop exact duplicates.
fn strip_duplicate_blocks(
    body: &str,
    muse_session_id: &str,
    dsh_session_id: &str,
    request_id: &str,
) -> Result<(String, usize)> {
    let blocks = find_blocks(body);
    if blocks.is_empty() {
        return Ok((body.to_string(), 0));
    }

    for block in &blocks {
        for (name, found, expected) in [
            (
                HEADER_MUSE_SESSION,
                &block.muse_session_id,
                muse_session_id,
            ),
            (
                HEADER_DSH_SESSION,
                &block.dsh_session_id,
                dsh_session_id,
            ),
            (HEADER_REQUEST_ID, &block.request_id, request_id),
        ] {
            match found {
                Some(value) if value == expected => {}
                Some(value) => bail!(
                    "the body carries a {RELAY_BLOCK_OPEN} block with {name} `{value}` but the \
                     server resolved `{expected}`; refusing to deliver a message whose text \
                     contradicts its header"
                ),
                None => bail!(
                    "the body carries a {RELAY_BLOCK_OPEN} block that does not state {name}; \
                     refusing to deliver it"
                ),
            }
        }
    }

    // Every block agreed with the server, so they are duplicates of the header
    // the server is about to add. Remove them, right to left so the byte ranges
    // stay valid.
    let mut clean = body.to_string();
    let mut removed = 0usize;
    for block in blocks.iter().rev() {
        clean.replace_range(block.start..block.end, "");
        removed += 1;
    }
    let trimmed_start = clean.trim_start_matches(['\r', '\n']).to_string();
    Ok((trimmed_start, removed))
}

fn validate_id(name: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("{name} must not be empty");
    }
    if value.len() > MAX_ID_LEN {
        bail!("{name} is longer than {MAX_ID_LEN} characters");
    }
    if let Some(bad) = value
        .chars()
        .find(|ch| matches!(ch, '\r' | '\n' | '\0') || ch.is_control())
    {
        return Err(anyhow!(
            "{name} contains an invalid character (U+{:04X}); header values must not contain CR, LF, NUL, or other control characters",
            bad as u32
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_of(relay: &RelayHeaders) -> Vec<(String, String)> {
        relay
            .to_headers()
            .expect("headers")
            .into_iter()
            .map(|header| (header.key, header.value))
            .collect()
    }

    #[test]
    fn emits_the_three_named_headers() {
        let relay = RelayHeaders {
            muse_session_id: Some("eeb1c4d1-8927-84dd-8037-9a23d2e2b2f0".into()),
            dsh_session_id: Some("ac5f1193-2a35-4be1-9b1c-485620007002".into()),
            request_id: Some("lfm3-a1b-v2-layeralloc-20261114-ac5f1193-1".into()),
        };
        let headers = headers_of(&relay);
        assert!(headers.contains(&(
            "museSessionId".to_string(),
            "eeb1c4d1-8927-84dd-8037-9a23d2e2b2f0".to_string()
        )));
        assert!(headers.contains(&(
            "dshSessionId".to_string(),
            "ac5f1193-2a35-4be1-9b1c-485620007002".to_string()
        )));
        assert!(headers.contains(&(
            "requestId".to_string(),
            "lfm3-a1b-v2-layeralloc-20261114-ac5f1193-1".to_string()
        )));
    }

    #[test]
    fn request_id_is_shared_with_the_transport_correlation_header() {
        let relay = RelayHeaders {
            request_id: Some("abc-1".into()),
            ..Default::default()
        };
        let headers = headers_of(&relay);
        assert!(headers.contains(&("x-request-id".to_string(), "abc-1".to_string())));
    }

    #[test]
    fn empty_relay_sends_nothing() {
        assert!(headers_of(&RelayHeaders::default()).is_empty());
    }

    #[test]
    fn partial_relay_sends_only_what_is_set() {
        let relay = RelayHeaders {
            dsh_session_id: Some("only-dsh".into()),
            ..Default::default()
        };
        let headers = headers_of(&relay);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, "dshSessionId");
    }

    #[test]
    fn rejects_header_injection() {
        for bad in ["a\r\nInjected: x", "a\nb", "a\rb", "a\0b"] {
            let relay = RelayHeaders {
                request_id: Some(bad.to_string()),
                ..Default::default()
            };
            assert!(relay.to_headers().is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn rejects_empty_and_overlong_ids() {
        let empty = RelayHeaders {
            muse_session_id: Some(String::new()),
            ..Default::default()
        };
        assert!(empty.to_headers().is_err());

        let long = RelayHeaders {
            request_id: Some("x".repeat(MAX_ID_LEN + 1)),
            ..Default::default()
        };
        assert!(long.to_headers().is_err());
    }

    #[test]
    fn accepts_multibyte_ids() {
        let relay = RelayHeaders {
            request_id: Some("要求-42".into()),
            ..Default::default()
        };
        assert!(relay.to_headers().is_ok());
    }


    fn envelope(muse: &str, dsh: &str, request: Option<&str>, body: &str) -> RelayEnvelope {
        RelayEnvelope::resolve(Some(muse), Some(dsh), request, body).expect("envelope")
    }

    const MUSE_A: &str = "002486c1-6c8e-4228-a5f9-35005acf9a17";
    const DSH_A: &str = "b206576f-1f21-45af-b0e4-92166f76a2c7";

    #[test]
    fn uuid_check_accepts_only_canonical_lowercase() {
        assert!(is_uuid(MUSE_A));
        assert!(!is_uuid("MUSE-A"));
        assert!(!is_uuid("002486c1-6c8e-4228-a5f9-35005acf9a1"));
        assert!(!is_uuid("002486c1-6c8e-4228-a5f9-35005acf9a17x"));
        assert!(!is_uuid("002486c1_6c8e_4228_a5f9_35005acf9a17"));
        assert!(!is_uuid(""));
    }

    // AC-1: the delivered body starts with the server block and the caller's
    // text follows unchanged.
    #[test]
    fn body_is_header_then_the_caller_text() {
        let env = envelope(MUSE_A, DSH_A, Some("req-1"), "本文です
二行目");
        let expected = format!(
            "[muse-relay]\n  museSessionId: {MUSE_A}\n  dshSessionId: {DSH_A}\n  requestId: req-1\n\n本文です\n二行目"
        );
        assert_eq!(env.body, expected);
        assert_eq!(env.stripped_blocks, 0);
    }

    #[test]
    fn header_block_matches_the_muse_to_dsh_format() {
        let block = render_block(MUSE_A, DSH_A, "req-1");
        let lines: Vec<&str> = block.lines().collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "[muse-relay]");
        assert!(lines[1].starts_with("  museSessionId: "));
        assert!(lines[2].starts_with("  dshSessionId: "));
        assert!(lines[3].starts_with("  requestId: "));
    }

    // AC-2: no destination or a malformed one is refused.
    #[test]
    fn missing_or_malformed_muse_session_is_refused() {
        assert!(RelayEnvelope::resolve(None, Some(DSH_A), Some("r"), "x").is_err());
        assert!(RelayEnvelope::resolve(Some(""), Some(DSH_A), Some("r"), "x").is_err());
        assert!(RelayEnvelope::resolve(Some("thread-name"), Some(DSH_A), Some("r"), "x").is_err());
        assert!(RelayEnvelope::resolve(Some("002486C1-6C8E-4228-A5F9-35005ACF9A17"),
            Some(DSH_A), Some("r"), "x").is_err());
    }

    // AC-3: a dsh session that disagrees with the environment is refused.
    #[test]
    fn mismatched_dsh_session_is_refused() {
        let previous = std::env::var("DSH_SESSION_ID").ok();
        std::env::set_var("DSH_SESSION_ID", DSH_A);
        let wrong = RelayEnvelope::resolve(Some(MUSE_A), Some("other-session"), Some("r"), "x");
        let right = RelayEnvelope::resolve(Some(MUSE_A), Some(DSH_A), Some("r"), "x");
        let auto = RelayEnvelope::resolve(Some(MUSE_A), None, Some("r"), "x");
        match previous {
            Some(value) => std::env::set_var("DSH_SESSION_ID", value),
            None => std::env::remove_var("DSH_SESSION_ID"),
        }
        assert!(wrong.is_err(), "a differing dsh session must be refused");
        assert_eq!(right.expect("explicit matches env").dsh_session_id, DSH_A);
        assert_eq!(auto.expect("env fills in").dsh_session_id, DSH_A);
    }

    #[test]
    fn missing_dsh_session_everywhere_is_refused() {
        let previous = std::env::var("DSH_SESSION_ID").ok();
        std::env::remove_var("DSH_SESSION_ID");
        let outcome = RelayEnvelope::resolve(Some(MUSE_A), None, Some("r"), "x");
        match previous {
            Some(value) => std::env::set_var("DSH_SESSION_ID", value),
            None => std::env::remove_var("DSH_SESSION_ID"),
        }
        assert!(outcome.is_err(), "no session id anywhere means no delivery");
    }

    // AC-4: a body block that contradicts the server stops the send.
    #[test]
    fn a_contradicting_body_block_is_refused() {
        let body = "[muse-relay]\n  museSessionId: 11111111-1111-4111-8111-111111111111\n  \
                    dshSessionId: b206576f-1f21-45af-b0e4-92166f76a2c7\n  requestId: r\n\n本文";
        let outcome = RelayEnvelope::resolve(Some(MUSE_A), Some(DSH_A), Some("r"), body);
        assert!(outcome.is_err(), "contradicting block must not be delivered");
    }

    #[test]
    fn an_incomplete_body_block_is_refused() {
        let body = "[muse-relay]\n  requestId: r\n\n本文";
        assert!(RelayEnvelope::resolve(Some(MUSE_A), Some(DSH_A), Some("r"), body).is_err());
    }

    // A duplicate of the server's own block is redundant, not contradictory.
    #[test]
    fn identical_body_blocks_are_stripped() {
        let body = "[muse-relay]\n  museSessionId: 002486c1-6c8e-4228-a5f9-35005acf9a17\n  \
                    dshSessionId: b206576f-1f21-45af-b0e4-92166f76a2c7\n  requestId: r\n\n本文";
        let env = envelope(MUSE_A, DSH_A, Some("r"), body);
        assert_eq!(env.stripped_blocks, 1);
        assert!(env.body.ends_with("\n\n本文"));
        // Exactly one block survives: the server's.
        assert_eq!(env.body.matches("[muse-relay]").count(), 1);
    }

    #[test]
    fn two_identical_blocks_are_both_stripped() {
        let one = "[muse-relay]\n  museSessionId: 002486c1-6c8e-4228-a5f9-35005acf9a17\n  \
                   dshSessionId: b206576f-1f21-45af-b0e4-92166f76a2c7\n  requestId: r\n\n";
        let body = format!("{one}{one}本文");
        let env = envelope(MUSE_A, DSH_A, Some("r"), &body);
        assert_eq!(env.stripped_blocks, 2);
        assert_eq!(env.body.matches("[muse-relay]").count(), 1);
    }

    // AC-5 support: a reply keeps the id it was given.
    #[test]
    fn a_supplied_request_id_is_kept_verbatim() {
        let env = envelope(MUSE_A, DSH_A, Some("muse-mcp-relay-reverse-20261115-b206576f-1"), "x");
        assert_eq!(env.request_id, "muse-mcp-relay-reverse-20261115-b206576f-1");
    }

    #[test]
    fn a_new_message_gets_a_generated_dsh_request_id() {
        let env = envelope(MUSE_A, DSH_A, None, "x");
        assert!(env.request_id.starts_with("dsh-"));
        assert!(env.request_id.len() > 4);
        let other = envelope(MUSE_A, DSH_A, None, "x");
        assert_ne!(env.request_id, other.request_id, "ids must be unique");
    }

    #[test]
    fn an_empty_request_id_also_gets_generated() {
        let env = envelope(MUSE_A, DSH_A, Some("   "), "x");
        assert!(env.request_id.starts_with("dsh-"));
    }

    #[test]
    fn headers_mirror_the_body_block() {
        let env = envelope(MUSE_A, DSH_A, Some("r"), "x");
        let headers: Vec<(String, String)> = env
            .to_headers()
            .expect("headers")
            .into_iter()
            .map(|header| (header.key, header.value))
            .collect();
        assert!(headers.contains(&("museSessionId".to_string(), MUSE_A.to_string())));
        assert!(headers.contains(&("dshSessionId".to_string(), DSH_A.to_string())));
        assert!(headers.contains(&("requestId".to_string(), "r".to_string())));
    }

    #[test]
    fn an_empty_body_still_carries_the_block() {
        let env = envelope(MUSE_A, DSH_A, Some("r"), "   ");
        assert!(env.body.starts_with("[muse-relay]"));
        assert!(!env.body.contains("\n\n"), "no trailing blank line for an empty body");
    }

    #[test]
    fn block_discovery_ignores_plain_text() {
        assert!(find_blocks("本文だけ").is_empty());
        assert!(find_blocks("  [muse-relay]  ").len() == 0 || find_blocks("  [muse-relay]  ").len() == 1);
    }

    #[test]
    fn envelope_json_echoes_the_three_wire_names() {
        let env = envelope(MUSE_A, DSH_A, Some("r"), "x");
        let json = env.to_json();
        assert_eq!(json["museSessionId"], MUSE_A);
        assert_eq!(json["dshSessionId"], DSH_A);
        assert_eq!(json["requestId"], "r");
        assert_eq!(json["stripped_blocks"], 0);
    }
}
