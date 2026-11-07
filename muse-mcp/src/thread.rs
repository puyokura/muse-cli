//! Thread (sub-chat) handling.
//!
//! Measured against the live gateway on 2026-11-07: there is no dedicated
//! "create thread" route (`/api/session/create`, `/api/thread/create`,
//! `/api/chats` are all 404). A thread is created as a side effect of
//! `/chat/stream` when the caller supplies its own `session_id`:
//!
//! ```text
//! POST /chat/stream {"items":[…], "session_id":"<client-uuid>"}
//!   -> {"is_thread": true, "session_id": "<client-uuid>", "message_id": "…"}
//! POST /chat/stream {"items":[…]}                       // no session_id
//!   -> {"is_thread": false, "session_id": "<primary>"}
//! ```
//!
//! So a stable name maps to a stable UUID, and the same name always continues
//! the same sub-chat. That keeps experiments out of the primary conversation
//! without any server-side bookkeeping.

use crate::client::MuseClient;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Namespace UUID used to derive thread ids from human names. Fixed forever so
/// `--thread lab` keeps pointing at the same sub-chat across runs.
const THREAD_NAMESPACE: [u8; 16] = [
    0x6d, 0x75, 0x73, 0x65, 0x2d, 0x74, 0x68, 0x72, 0x65, 0x61, 0x64, 0x2d, 0x6e, 0x73, 0x00,
    0x01,
];

/// Derive the session id for a named thread.
///
/// The gateway only requires a well-formed UUID, so this is a name-derived
/// identifier built from SHA-256 (already a dependency) and tagged as an
/// RFC 9562 version-8 "custom" UUID rather than claiming to be a UUIDv5. The
/// important property is determinism: the same name always yields the same id.
pub fn thread_id(name: &str) -> String {
    use sha2::{Digest, Sha256};

    let trimmed = name.trim();
    let mut hasher = Sha256::new();
    hasher.update(THREAD_NAMESPACE);
    hasher.update(trimmed.as_bytes());
    let digest = hasher.finalize();

    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // Version 8 (custom, RFC 9562) and the RFC 4122 variant bits.
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5],
        bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

/// One row from `/api/session/list`.
#[derive(Debug, Clone)]
pub struct ThreadSummary {
    pub session_id: String,
    pub title: String,
    pub is_primary: bool,
    pub is_thread: bool,
    pub archived: bool,
    pub pinned: bool,
    pub updated_at_ms: i64,
    pub snippet: String,
}

fn opt_str(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn opt_bool(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn opt_i64(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or_default()
}

/// Parse `/api/session/list` into summaries, newest first.
pub fn parse_sessions(payload: &Value) -> Vec<ThreadSummary> {
    let array = payload
        .get("sessions")
        .and_then(Value::as_array)
        .or_else(|| payload.get("result")?.get("sessions")?.as_array());
    let Some(array) = array else {
        return Vec::new();
    };
    let mut rows: Vec<ThreadSummary> = array
        .iter()
        .map(|entry| ThreadSummary {
            session_id: opt_str(entry, "session_id"),
            title: opt_str(entry, "title"),
            is_primary: opt_bool(entry, "is_primary"),
            is_thread: opt_bool(entry, "is_thread"),
            archived: opt_bool(entry, "archived"),
            pinned: opt_bool(entry, "pinned"),
            updated_at_ms: opt_i64(entry, "updated_at_ms"),
            snippet: opt_str(entry, "snippet"),
        })
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.updated_at_ms));
    rows
}

/// Look up one thread by name, returning the server row when it exists.
pub async fn find_by_name(client: &MuseClient, name: &str) -> Result<Option<ThreadSummary>> {
    let wanted = thread_id(name);
    let sessions = parse_sessions(&client.sessions().await?);
    Ok(sessions.into_iter().find(|row| row.session_id == wanted))
}

/// Resolve `--thread NAME` to a session id.
///
/// `None` means "the primary conversation". A name that has not been used yet
/// resolves to its derived id but is reported as `exists: false` so callers can
/// decide whether to create it instead of silently attaching to a dead id.
pub struct ResolvedThread {
    pub session_id: Option<String>,
    pub exists: bool,
    pub row: Option<ThreadSummary>,
}

impl ResolvedThread {
    /// Human label for this target: the server title when known, the raw id
    /// otherwise, and `primary` when no name was given.
    pub fn label(&self) -> String {
        if let Some(row) = &self.row {
            return crate::sysfs::clean_title(&row.title);
        }
        match &self.session_id {
            Some(id) => id[..id.len().min(8)].to_string(),
            None => "primary".to_string(),
        }
    }
}

pub async fn resolve(client: &MuseClient, name: Option<&str>) -> Result<ResolvedThread> {
    let Some(name) = name else {
        return Ok(ResolvedThread {
            session_id: None,
            exists: true,
            row: None,
        });
    };
    if name.trim().is_empty() {
        bail!("thread name must not be empty");
    }
    let id = thread_id(name);
    let sessions = parse_sessions(&client.sessions().await?);
    let row = sessions.into_iter().find(|entry| entry.session_id == id);
    Ok(ResolvedThread {
        exists: row.is_some(),
        row,
        session_id: Some(id),
    })
}

/// Ensure a named thread exists, creating it with a short opening prompt when
/// the gateway has never seen the derived id.
pub async fn ensure(
    client: &MuseClient,
    name: &str,
    opening_prompt: Option<&str>,
    timeout: std::time::Duration,
) -> Result<ThreadSummary> {
    if let Some(existing) = find_by_name(client, name).await? {
        return Ok(existing);
    }
    let id = thread_id(name);
    let prompt = opening_prompt
        .map(str::to_string)
        .unwrap_or_else(|| format!("このスレッドは「{name}」専用です。以降はこのスレッドで作業します。"));

    let outcome = client
        .ask_and_wait(&prompt, Some(&id), &[], timeout, std::time::Duration::from_secs(2))
        .await?;

    // The server derives a title from the prompt; replace it with the caller's
    // name so the thread is recognisable in the app.
    let _ = client
        .raw_rpc(
            "POST",
            "/api/session/rename",
            Some(json!({ "session_id": id, "title": format!("[{name}]") })),
        )
        .await;

    Ok(ThreadSummary {
        session_id: id,
        title: name.to_string(),
        is_primary: false,
        is_thread: true,
        archived: false,
        pinned: false,
        updated_at_ms: 0,
        snippet: outcome
            .reply
            .get("display_text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// Group the primary conversation and every side thread for display.
pub fn summarise(rows: &[ThreadSummary]) -> Value {
    let primary: Vec<&ThreadSummary> = rows.iter().filter(|row| row.is_primary).collect();
    let threads: Vec<&ThreadSummary> = rows
        .iter()
        .filter(|row| !row.is_primary && row.is_thread)
        .collect();
    let other: Vec<&ThreadSummary> = rows
        .iter()
        .filter(|row| !row.is_primary && !row.is_thread)
        .collect();

    let render = |row: &ThreadSummary| {
        json!({
            "session_id": row.session_id,
            "title": row.title,
            "archived": row.archived,
            "pinned": row.pinned,
            "updated_at_ms": row.updated_at_ms,
            "snippet": row.snippet,
        })
    };

    json!({
        "primary": primary.iter().map(|row| render(row)).collect::<Vec<_>>(),
        "threads": threads.iter().map(|row| render(row)).collect::<Vec<_>>(),
        "other": other.iter().map(|row| render(row)).collect::<Vec<_>>(),
        "counts": {
            "primary": primary.len(),
            "threads": threads.len(),
            "other": other.len(),
        },
    })
}

/// Map of derived-id -> name for every thread this client has created, so the
/// CLI can show `--thread` names beside opaque ids without extra storage.
pub fn name_index(rows: &[ThreadSummary]) -> BTreeMap<String, String> {
    rows.iter()
        .filter(|row| row.is_thread && !row.title.is_empty())
        .map(|row| (row.session_id.clone(), row.title.clone()))
        .collect()
}

/// Reject names the CLI cannot turn into a stable thread.
pub fn validate_name(name: &str) -> Result<()> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("thread name must not be empty");
    }
    if trimmed.chars().count() > 64 {
        bail!("thread name is limited to 64 characters");
    }
    if trimmed.chars().any(|ch| ch == '\n' || ch == '\r') {
        bail!("thread name must be a single line");
    }
    Ok(())
}


/// Convenience wrapper used by the CLI to normalise `--thread` input.
pub fn normalise(name: Option<&str>) -> Result<Option<String>> {
    match name {
        None => Ok(None),
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            validate_name(trimmed)?;
            Ok(Some(trimmed.to_string()))
        }
    }
}

/// Error helper so callers report a missing thread instead of creating one
/// implicitly.
pub fn missing_error(name: &str) -> anyhow::Error {
    anyhow!(
        "thread `{name}` has not been created yet; run `muse thread new {name}` first \
         (implicit creation is disabled so experiments never land in an unexpected place)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_id_is_deterministic() {
        assert_eq!(thread_id("lab"), thread_id("lab"));
        assert_eq!(thread_id(" lab "), thread_id("lab"));
    }

    #[test]
    fn different_names_differ() {
        assert_ne!(thread_id("lab"), thread_id("lab2"));
    }

    #[test]
    fn derived_id_is_a_well_formed_uuid() {
        let id = thread_id("mcp-lab");
        assert_eq!(id.len(), 36);
        let groups: Vec<&str> = id.split('-').collect();
        assert_eq!(groups.len(), 5);
        assert_eq!(groups.iter().map(|g| g.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
        // Version 8 marker and RFC 4122 variant.
        assert_eq!(&id[14..15], "8");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
        assert!(id.chars().all(|ch| ch.is_ascii_hexdigit() || ch == '-'));
    }

    #[test]
    fn non_ascii_names_are_supported() {
        let id = thread_id("実験スレッド");
        assert_eq!(id.len(), 36);
        assert_eq!(thread_id("実験スレッド"), id);
    }

    #[test]
    fn parses_session_rows_and_sorts_newest_first() {
        let payload = serde_json::json!({
            "sessions": [
                {"session_id": "a", "title": "old", "is_primary": false, "is_thread": true, "updated_at_ms": 10},
                {"session_id": "b", "title": "new", "is_primary": false, "is_thread": true, "updated_at_ms": 20},
                {"session_id": "p", "title": "primary", "is_primary": true, "is_thread": false, "updated_at_ms": 5},
            ]
        });
        let rows = parse_sessions(&payload);
        assert_eq!(rows[0].session_id, "b");
        assert_eq!(rows[1].session_id, "a");
        assert_eq!(rows[2].session_id, "p");
        assert!(rows[2].is_primary);
        assert!(rows[0].is_thread);
    }

    #[test]
    fn summary_separates_primary_from_threads() {
        let rows = vec![
            ThreadSummary {
                session_id: "p".into(),
                title: "primary".into(),
                is_primary: true,
                is_thread: false,
                archived: false,
                pinned: false,
                updated_at_ms: 0,
                snippet: String::new(),
            },
            ThreadSummary {
                session_id: "t".into(),
                title: "thread".into(),
                is_primary: false,
                is_thread: true,
                archived: false,
                pinned: false,
                updated_at_ms: 0,
                snippet: String::new(),
            },
        ];
        let summary = summarise(&rows);
        assert_eq!(summary["counts"]["primary"], 1);
        assert_eq!(summary["counts"]["threads"], 1);
        assert_eq!(summary["counts"]["other"], 0);
        assert_eq!(summary["primary"][0]["session_id"], "p");
        assert_eq!(summary["threads"][0]["session_id"], "t");
    }

    #[test]
    fn rejects_unusable_names() {
        assert!(validate_name("lab").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("a\nb").is_err());
        assert!(validate_name(&"x".repeat(65)).is_err());
    }

    #[test]
    fn normalise_trims_and_rejects_empty() {
        assert_eq!(normalise(None).unwrap(), None);
        assert_eq!(normalise(Some("   ")).unwrap(), None);
        assert_eq!(normalise(Some(" lab ")).unwrap(), Some("lab".to_string()));
    }

    #[test]
    fn missing_error_names_the_command() {
        let message = format!("{}", missing_error("lab"));
        assert!(message.contains("muse thread new lab"));
    }
}
