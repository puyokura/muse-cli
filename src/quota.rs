//! Quota and subscription state from `/hatch/subscription`.
//!
//! The payload carries more than a percentage: the tier, the reset moment,
//! whether top-ups are allowed, and the labels the app shows. A number alone
//! invites the wrong reading ("64% used" of what, and until when), so the
//! summary states the denominator and the reset time next to it.

use serde_json::Value;

/// A one-line-per-field summary of the subscription payload.
///
/// Unknown or absent fields are printed as `-` rather than omitted: a missing
/// line would read as "not applicable", which is a different claim.
pub fn summarize(value: &Value) -> String {
    let get = |path: &[&str]| -> Option<&Value> {
        let mut current = value;
        for key in path {
            current = current.get(key)?;
        }
        Some(current)
    };
    let text = |path: &[&str]| -> String {
        match get(path) {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::Bool(b)) => b.to_string(),
            Some(Value::Null) | None => "-".to_string(),
            Some(other) => other.to_string(),
        }
    };

    let mut lines = Vec::new();
    lines.push(format!(
        "tier            {} ({})",
        text(&["tier", "name"]),
        text(&["tier", "tier_code"])
    ));
    lines.push(format!(
        "subscription    {} paid={} status={}",
        text(&["subscription_type"]),
        get(&["tier", "is_paid"])
            .and_then(Value::as_bool)
            .map(|b| b.to_string())
            .unwrap_or_else(|| "-".to_string()),
        text(&["status_subtitle"])
    ));

    let percent = get(&["usage", "percent_used"])
        .and_then(Value::as_i64)
        .map(|p| format!("{p}%"))
        .unwrap_or_else(|| "-".to_string());
    lines.push(format!(
        "usage           {} of {}  state={} status={}",
        percent,
        text(&["usage_row_label"]),
        text(&["usage", "state"]),
        text(&["usage", "quota_status"])
    ));

    let reset = get(&["usage", "resets_at"])
        .and_then(Value::as_i64)
        .map(format_unix_seconds)
        .unwrap_or_else(|| "-".to_string());
    lines.push(format!("resets_at       {reset}"));

    if let Some(bp) = get(&["usage", "overspend_basis_points"]).and_then(Value::as_i64) {
        lines.push(format!("overspend       {bp} basis points"));
    }

    let topup = get(&["topup_balance"])
        .and_then(Value::as_i64)
        .map(|b| format_thousands(b))
        .unwrap_or_else(|| "-".to_string());
    let total = get(&["topup_total"])
        .and_then(Value::as_i64)
        .map(|b| format_thousands(b))
        .unwrap_or_else(|| "-".to_string());
    lines.push(format!(
        "topup           {topup} / {total}  {}  eligible={}",
        text(&["topup_row_value_label"]),
        text(&["topup_eligible"])
    ));

    lines.push(format!(
        "catalog         upgrade_eligible={} invite_reward={} cta={}",
        text(&["upgrade_eligible"]),
        text(&["invite_reward_eligible"]),
        text(&["entry_point_cta_label"])
    ));

    lines.join("\n")
}

/// Render a unix-seconds timestamp as UTC, without depending on a date crate.
///
/// The app shows this moment to the user, so printing the raw integer would be
/// less useful than the one conversion that needs no timezone data.
pub fn format_unix_seconds(seconds: i64) -> String {
    if seconds <= 0 {
        return "-".to_string();
    }
    // Days since the epoch to a civil date, using the standard 400/100/4/1 cycle.
    let days = seconds / 86_400;
    let mut remaining_year = days;
    let mut year = 1970i64;
    loop {
        let leap = is_leap(year);
        let length = if leap { 366 } else { 365 };
        if remaining_year < length {
            break;
        }
        remaining_year -= length;
        year += 1;
    }
    let month_lengths = if is_leap(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month = 0usize;
    while month < 12 && remaining_year >= month_lengths[month] {
        remaining_year -= month_lengths[month];
        month += 1;
    }
    let day = remaining_year + 1;
    let second_of_day = seconds % 86_400;
    format!(
        "{year}-{:02}-{:02} {:02}:{:02}:{:02}Z",
        month + 1,
        day,
        second_of_day / 3600,
        (second_of_day % 3600) / 60,
        second_of_day % 60
    )
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Group digits in threes so a token count is readable at a glance.
pub fn format_thousands(value: i64) -> String {
    let negative = value < 0;
    let digits = value.abs().to_string();
    let mut out = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if negative {
        format!("-{out}")
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload(percent: i64) -> Value {
        json!({
            "subscription_type": "FREE",
            "status_subtitle": "Weekly limit resets on Nov 18",
            "tier": {"name": "Muse Free", "tier_code": "HATCH_FREE", "is_paid": false},
            "usage": {"percent_used": percent, "quota_status": "SUFFICIENT",
                      "resets_at": 1795043657, "state": "METERED"},
            "usage_row_label": "Free plan",
            "topup_balance": 80000000,
            "topup_total": 80000000,
            "topup_row_value_label": "0% used (2B tokens left)",
            "topup_eligible": false,
            "upgrade_eligible": true,
            "invite_reward_eligible": true,
            "entry_point_cta_label": "Upgrade"
        })
    }

    #[test]
    fn summary_reports_the_percentage_with_its_denominator() {
        let text = summarize(&payload(64));
        assert!(text.contains("64%"), "{text}");
        // The denominator is what makes 64% meaningful.
        assert!(text.contains("of Free plan"), "{text}");
        assert!(text.contains("HATCH_FREE"), "{text}");
    }

    #[test]
    fn summary_includes_reset_and_topup() {
        let text = summarize(&payload(64));
        assert!(text.contains("2026-"), "reset time is missing: {text}");
        assert!(text.contains("80,000,000"), "topup balance is not grouped: {text}");
        assert!(text.contains("0% used (2B tokens left)"), "{text}");
    }

    #[test]
    fn missing_fields_are_dashes_not_omitted() {
        let text = summarize(&json!({}));
        // Six lines are unconditional; overspend is added only when the payload
        // carries it, so an empty payload must still print the other six.
        assert_eq!(text.lines().count(), 6, "every base line must stay present: {text}");
        for prefix in ["tier", "subscription", "usage", "resets_at", "topup", "catalog"] {
            assert!(
                text.lines().any(|line| line.starts_with(prefix)),
                "{prefix} line is missing: {text}"
            );
        }
        // "-" means "the payload did not carry it", which is different from
        // hiding the line and leaving the reader to guess.
        assert!(text.contains("usage           - of -"), "{text}");
    }

    #[test]
    fn unix_seconds_to_utc() {
        assert_eq!(format_unix_seconds(0), "-");
        assert_eq!(format_unix_seconds(1_600_000_000), "2020-09-13 12:26:40Z");
        // Leap day: 2024-02-29.
        assert_eq!(format_unix_seconds(1_709_164_800), "2024-02-29 00:00:00Z");
    }

    #[test]
    fn thousands_grouping() {
        assert_eq!(format_thousands(0), "0");
        assert_eq!(format_thousands(999), "999");
        assert_eq!(format_thousands(1_000), "1,000");
        assert_eq!(format_thousands(80_000_000), "80,000,000");
        assert_eq!(format_thousands(-1_234), "-1,234");
    }
}
