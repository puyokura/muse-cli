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
    /// True when no relay header would be sent.
    pub fn is_empty(&self) -> bool {
        self.muse_session_id.is_none()
            && self.dsh_session_id.is_none()
            && self.request_id.is_none()
    }

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

    /// The correlation triple, for echoing back to the caller.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "muse_session_id": self.muse_session_id,
            "dsh_session_id": self.dsh_session_id,
            "request_id": self.request_id,
        })
    }
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
        assert!(RelayHeaders::default().is_empty());
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

    #[test]
    fn json_echo_keeps_all_three_fields() {
        let relay = RelayHeaders {
            muse_session_id: Some("m".into()),
            dsh_session_id: None,
            request_id: Some("r".into()),
        };
        let json = relay.to_json();
        assert_eq!(json["muse_session_id"], "m");
        assert!(json["dsh_session_id"].is_null());
        assert_eq!(json["request_id"], "r");
    }
}
