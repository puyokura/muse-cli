//! Health checks that answer "why is this not working" without a terminal.
//!
//! Each check is independent and reports its own result, so a failure names the
//! layer that broke instead of collapsing into one error. Nothing here needs a
//! terminal or changes console state.

use crate::client::MuseClient;
use anyhow::Result;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

#[derive(Clone, Copy)]
pub enum Status {
    Ok,
    Warn,
    Fail,
    Skip,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "fail",
            Status::Skip => "skip",
        }
    }

    /// Whether this status should make the overall result fail.
    fn is_failure(self) -> bool {
        matches!(self, Status::Fail)
    }
}

pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
}

impl Check {
    fn new(name: &'static str, status: Status, detail: impl Into<String>) -> Self {
        Self {
            name,
            status,
            detail: detail.into(),
        }
    }
}

/// Everything the doctor looked at.
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    pub fn failed(&self) -> bool {
        self.checks.iter().any(|check| check.status.is_failure())
    }

    pub fn to_json(&self) -> Value {
        let checks: Vec<Value> = self
            .checks
            .iter()
            .map(|check| {
                json!({
                    "name": check.name,
                    "status": check.status.label(),
                    "detail": check.detail,
                })
            })
            .collect();
        // Count each status once. A simple match is clearer than walking the
        // JSON tree to find the slot.
        let mut counts = json!({"ok": 0, "warn": 0, "fail": 0, "skip": 0});
        for check in &self.checks {
            let key = check.status.label();
            let next = counts[key].as_u64().unwrap_or(0) + 1;
            counts[key] = json!(next);
        }
        json!({
            "ok": !self.failed(),
            "counts": counts,
            "checks": checks,
        })
    }

    /// One line per check, dense enough to scan.
    pub fn to_text(&self) -> String {
        let width = self
            .checks
            .iter()
            .map(|check| check.name.len())
            .max()
            .unwrap_or(0);
        self.checks
            .iter()
            .map(|check| {
                format!(
                    "{:<width$}  {:<4}  {}",
                    check.name,
                    check.status.label(),
                    check.detail,
                    width = width
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Run every check. `live` adds the ones that touch the network.
pub async fn diagnose(client: &Arc<MuseClient>, live: bool) -> Result<Report> {
    let mut checks = Vec::new();

    checks.push(check_state_file(client.state_path()));
    checks.push(check_device_id(client.state_path()));

    // Authentication against muse.ai does not use the gateway, so it can pass
    // while the VM link is down. That distinction mattered: a caller that only
    // ran `status` concluded "half alive" when the socket was dead.
    let auth = {
        let mut http = client.http_mut().await;
        http.auth_check().await
    };
    checks.push(match &auth {
        Ok(value) => {
            let signed_in = value.get("ok").and_then(Value::as_bool).unwrap_or(false);
            let outcome = value
                .get("outcome")
                .and_then(Value::as_str)
                .unwrap_or("-");
            let viewer = value
                .get("viewer_id")
                .and_then(Value::as_str)
                .unwrap_or("-");
            if signed_in {
                Check::new(
                    "auth",
                    Status::Ok,
                    format!("{outcome}, viewer {viewer}"),
                )
            } else {
                Check::new(
                    "auth",
                    Status::Fail,
                    format!("not signed in ({outcome}); run `muse login`"),
                )
            }
        }
        Err(err) => Check::new(
            "auth",
            Status::Fail,
            format!("auth_check failed: {err}; run `muse login`"),
        ),
    });

    if !live {
        checks.push(Check::new(
            "gateway",
            Status::Skip,
            "not checked (pass --live)",
        ));
        return Ok(Report { checks });
    }

    // The gateway round-trip is the one that says whether tools can work at all.
    checks.push(match client.ping().await {
        Ok(value) => {
            let ts = value
                .get("server_ts_ms")
                .and_then(Value::as_i64)
                .map(|ms| ms.to_string())
                .unwrap_or_else(|| "-".to_string());
            Check::new("gateway", Status::Ok, format!("ping ok, server_ts_ms {ts}"))
        }
        Err(err) => Check::new(
            "gateway",
            Status::Fail,
            format!("{err}  (every tool goes through this)"),
        ),
    });

    if client.ping().await.is_ok() {
        checks.push(match client.model().await {
            Ok(value) => {
                // The descriptor is `{default_model, available_models[]}`, not a
                // bare `model` field; reading the wrong key reported "?" while
                // the call itself succeeded.
                let name = value
                    .get("default_model")
                    .and_then(Value::as_str)
                    .or_else(|| value.get("model").and_then(Value::as_str))
                    .unwrap_or("unknown");
                let count = value
                    .get("available_models")
                    .and_then(Value::as_array)
                    .map(|rows| rows.len())
                    .unwrap_or(0);
                Check::new(
                    "model",
                    Status::Ok,
                    format!("{name} ({count} available)"),
                )
            }
            Err(err) => Check::new("model", Status::Warn, format!("{err}")),
        });
        checks.push(match client.sessions().await {
            Ok(value) => {
                let count = value
                    .get("sessions")
                    .and_then(Value::as_array)
                    .map(|rows| rows.len())
                    .unwrap_or(0);
                Check::new("sessions", Status::Ok, format!("{count} conversations listed"))
            }
            Err(err) => Check::new("sessions", Status::Warn, format!("{err}")),
        });
    } else {
        checks.push(Check::new(
            "model",
            Status::Skip,
            "skipped: the gateway is down",
        ));
        checks.push(Check::new(
            "sessions",
            Status::Skip,
            "skipped: the gateway is down",
        ));
    }

    Ok(Report { checks })
}

fn check_state_file(path: &Path) -> Check {
    if !path.exists() {
        return Check::new(
            "state",
            Status::Fail,
            format!("{} does not exist; run `muse login`", path.display()),
        );
    }
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) => {
            return Check::new(
                "state",
                Status::Fail,
                format!("cannot stat {}: {err}", path.display()),
            )
        }
    };
    if metadata.len() == 0 {
        return Check::new(
            "state",
            Status::Fail,
            format!("{} is empty; run `muse login`", path.display()),
        );
    }
    let detail = format!("{} bytes at {}", metadata.len(), path.display());
    // On Unix the file holds credentials, so anything group- or world-readable
    // is worth saying out loud even though the write path sets 0600.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Check::new(
                "state",
                Status::Warn,
                format!("{detail}, mode {mode:04o} is wider than 0600"),
            );
        }
        return Check::new("state", Status::Ok, format!("{detail}, mode {mode:04o}"));
    }
    #[cfg(not(unix))]
    Check::new("state", Status::Ok, detail)
}

fn check_device_id(state_path: &Path) -> Check {
    let marker = state_path.with_file_name("device.json");
    match std::fs::read_to_string(&marker) {
        Ok(raw) => match serde_json::from_str::<Value>(&raw) {
            Ok(value) => {
                let id = value
                    .get("device_id")
                    .and_then(Value::as_str)
                    .unwrap_or("-");
                Check::new("device", Status::Ok, format!("present ({})", short(id)))
            }
            Err(err) => Check::new(
                "device",
                Status::Warn,
                format!("{} is not valid JSON: {err}", marker.display()),
            ),
        },
        Err(_) => Check::new(
            "device",
            Status::Warn,
            format!("{} missing; it is created on first use", marker.display()),
        ),
    }
}

fn short(id: &str) -> String {
    if id.len() > 12 {
        format!("{}…", &id[..8])
    } else {
        id.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(checks: Vec<Check>) -> Report {
        Report { checks }
    }

    #[test]
    fn a_failure_makes_the_report_fail() {
        let report = report(vec![
            Check::new("state", Status::Ok, "fine"),
            Check::new("auth", Status::Fail, "not signed in"),
        ]);
        assert!(report.failed());
        assert_eq!(report.to_json()["ok"], false);
    }

    #[test]
    fn warnings_alone_do_not_fail() {
        let report = report(vec![
            Check::new("state", Status::Ok, "fine"),
            Check::new("device", Status::Warn, "missing"),
            Check::new("gateway", Status::Skip, "not checked"),
        ]);
        assert!(!report.failed(), "warn and skip are not failures");
        assert_eq!(report.to_json()["ok"], true);
        assert_eq!(report.to_json()["counts"]["warn"], 1);
        assert_eq!(report.to_json()["counts"]["skip"], 1);
    }

    #[test]
    fn text_output_is_one_aligned_line_per_check() {
        // The markers are distinct from any status word so the column offset
        // this test measures cannot accidentally match inside "warn" or "fail".
        let report = report(vec![
            Check::new("state", Status::Ok, "detail-A"),
            Check::new("gateway", Status::Fail, "detail-B"),
        ]);
        let text = report.to_text();
        assert_eq!(text.lines().count(), 2);
        let positions: Vec<usize> = text
            .lines()
            .map(|line| line.find("detail-").expect("marker"))
            .collect();
        assert_eq!(positions[0], positions[1], "detail columns must align:\n{text}");
    }

    #[test]
    fn missing_state_file_is_a_failure() {
        let path = std::env::temp_dir().join("muse-doctor-absent.json");
        let _ = std::fs::remove_file(&path);
        let check = check_state_file(&path);
        assert!(matches!(check.status, Status::Fail));
        assert!(check.detail.contains("muse login"));
    }

    #[test]
    fn empty_state_file_is_a_failure() {
        let path = std::env::temp_dir().join("muse-doctor-empty.json");
        std::fs::write(&path, b"").expect("write");
        let check = check_state_file(&path);
        let _ = std::fs::remove_file(&path);
        assert!(matches!(check.status, Status::Fail));
    }

    #[cfg(unix)]
    #[test]
    fn a_wide_state_mode_is_a_warning() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join("muse-doctor-wide.json");
        std::fs::write(&path, b"{}").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let check = check_state_file(&path);
        let _ = std::fs::remove_file(&path);
        assert!(matches!(check.status, Status::Warn), "{}", check.detail);
        assert!(check.detail.contains("0600"));
    }
}
