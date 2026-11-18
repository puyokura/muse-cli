//! Exact token counters.
//!
//! The subscription payload carries a percentage and nothing finer: the DEX
//! shows `ViewerUsageJson` holding only `state`, `percent_used`, and
//! `quota_status`, and no route on either host returns raw usage numbers.
//! The counters do exist, but only in the reply to the app's `/status` command,
//! which is where this reads them from.
//!
//! Two kinds of number therefore live here, and they must not be mixed:
//! counters parsed from `/status` (exact integers), and a token figure derived
//! from the top-up credit balance (derived, with its basis stated).

use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Tokens per top-up credit, verified against all four packs in
/// `/hatch/subscription/catalog` (20M→500M, 80M→2B, 200M→5B, 480M→12B).
pub const TOKENS_PER_CREDIT: i64 = 25;

/// Fields `/status` reports. Keep them in one place so a parser test and the
/// summary cannot drift apart.
pub const COUNTERS: &[&str] = &[
    "total_input_tokens",
    "total_output_tokens",
    "session_input_tokens",
    "session_output_tokens",
    "session_subagent_input_tokens",
    "session_subagent_output_tokens",
    "session_aggregate_input_tokens",
    "session_aggregate_output_tokens",
    "context_used_tokens",
    "context_window_tokens",
    "context_available_tokens",
    "context_used_percent",
    "context_available_percent",
];

/// A parsed `/status` reply.
#[derive(Default)]
pub struct Status {
    /// String-valued fields, e.g. the model name.
    pub text: BTreeMap<String, String>,
    /// Integer counters, exact as reported.
    pub numbers: BTreeMap<String, i64>,
}

impl Status {
    pub fn get(&self, key: &str) -> Option<i64> {
        self.numbers.get(key).copied()
    }
}

/// Parse the `key=value ...` line the command returns.
///
/// One value can contain spaces (`model=Muse Spark`), so a token without `=`
/// continues the previous key rather than opening a new field. Dropping it
/// instead would lose half the model name and silently mislabel the output.
pub fn parse_status(reply: &str) -> Status {
    let mut status = Status::default();
    let mut last_key: Option<String> = None;
    for token in reply.split_whitespace() {
        match token.split_once('=') {
            Some((key, value)) => {
                let key = key.trim();
                if key.is_empty() {
                    continue;
                }
                match value.trim().parse::<i64>() {
                    Ok(number) => {
                        status.numbers.insert(key.to_string(), number);
                    }
                    Err(_) => {
                        status.text.insert(key.to_string(), value.trim().to_string());
                    }
                }
                last_key = Some(key.to_string());
            }
            None => {
                // Continuation of the previous value: "Muse" then "Spark".
                if let Some(key) = &last_key {
                    if let Some(existing) = status.text.get_mut(key) {
                        existing.push(' ');
                        existing.push_str(token);
                    }
                }
            }
        }
    }
    status
}

/// The figures the summary needs from the subscription payload.
pub struct Quota {
    pub percent_used: Option<i64>,
    pub quota_status: Option<String>,
    pub resets_at: Option<i64>,
    pub tier: Option<String>,
    pub topup_balance: Option<i64>,
    pub topup_total: Option<i64>,
}

impl Quota {
    pub fn from_subscription(value: &Value) -> Self {
        let text = |path: &[&str]| -> Option<String> {
            let mut current = value;
            for key in path {
                current = current.get(key)?;
            }
            current.as_str().map(str::to_string)
        };
        let number = |path: &[&str]| -> Option<i64> {
            let mut current = value;
            for key in path {
                current = current.get(key)?;
            }
            current.as_i64()
        };
        Self {
            percent_used: number(&["usage", "percent_used"]),
            quota_status: text(&["usage", "quota_status"]),
            resets_at: number(&["usage", "resets_at"]),
            tier: text(&["tier", "name"]),
            topup_balance: number(&["topup_balance"]),
            topup_total: number(&["topup_total"]),
        }
    }
}

fn grouped(value: i64) -> String {
    crate::quota::format_thousands(value)
}

/// Dense summary: one line per group, then the limitation.
///
/// The percentage has no token equivalent, and saying so is part of the answer:
/// printing `64%` beside exact counters invites the reader to convert, and the
/// conversion is not available from anything this client can read.
pub fn summarize(status: &Status, quota: &Quota) -> String {
    let model = status.text.get("model").cloned().unwrap_or_else(|| "-".into());
    let mut lines = Vec::new();
    lines.push(format!("model           {model}"));

    let total_in = status.get("total_input_tokens");
    let total_out = status.get("total_output_tokens");
    lines.push(format!(
        "lifetime        in {}  out {}  (account, all sessions)",
        total_in.map(grouped).unwrap_or_else(|| "-".into()),
        total_out.map(grouped).unwrap_or_else(|| "-".into())
    ));

    lines.push(format!(
        "session         in {}  out {}  (this conversation)",
        status
            .get("session_input_tokens")
            .map(grouped)
            .unwrap_or_else(|| "-".into()),
        status
            .get("session_output_tokens")
            .map(grouped)
            .unwrap_or_else(|| "-".into())
    ));

    let sub_in = status.get("session_subagent_input_tokens").unwrap_or(0);
    let sub_out = status.get("session_subagent_output_tokens").unwrap_or(0);
    lines.push(format!(
        "subagents       in {}  out {}",
        grouped(sub_in),
        grouped(sub_out)
    ));

    let agg_in = status.get("session_aggregate_input_tokens");
    let agg_out = status.get("session_aggregate_output_tokens");
    lines.push(format!(
        "aggregate       in {}  out {}  (session + subagents)",
        agg_in.map(grouped).unwrap_or_else(|| "-".into()),
        agg_out.map(grouped).unwrap_or_else(|| "-".into())
    ));

    let used = status.get("context_used_tokens");
    let window = status.get("context_window_tokens");
    let used_pct = status.get("context_used_percent");
    lines.push(format!(
        "context         used {} / {}  ({}%)",
        used.map(grouped).unwrap_or_else(|| "-".into()),
        window.map(grouped).unwrap_or_else(|| "-".into()),
        used_pct.map(|p| p.to_string()).unwrap_or_else(|| "-".into())
    ));

    let available = status.get("context_available_tokens");
    let available_pct = status.get("context_available_percent");
    lines.push(format!(
        "available       {}  ({}%)",
        available.map(grouped).unwrap_or_else(|| "-".into()),
        available_pct.map(|p| p.to_string()).unwrap_or_else(|| "-".into())
    ));

    let percent = quota
        .percent_used
        .map(|p| format!("{p}%"))
        .unwrap_or_else(|| "-".into());
    let reset = quota
        .resets_at
        .map(crate::quota::format_unix_seconds)
        .unwrap_or_else(|| "-".into());
    lines.push(format!(
        "quota           {} of {}  status {}  resets {}",
        percent,
        quota.tier.clone().unwrap_or_else(|| "-".into()),
        quota.quota_status.clone().unwrap_or_else(|| "-".into()),
        reset
    ));

    match (quota.topup_balance, quota.topup_total) {
        (Some(balance), Some(total)) => {
            let glide = TOKENS_PER_CREDIT;
            lines.push(format!(
                "topup           {} / {} credits  = {} / {} tokens ({glide}x)",
                grouped(balance),
                grouped(total),
                grouped(balance.saturating_mul(glide)),
                grouped(total.saturating_mul(glide))
            ));
        }
        _ => lines.push("topup           -".to_string()),
    }

    lines.push(
        "note            the weekly cap is not exposed, so the quota percentage has \
         no token equivalent; the counters above are the exact figures"
            .to_string(),
    );
    lines.join("\n")
}

/// Machine-readable form. Derived values carry the basis next to them so a
/// consumer cannot mistake them for measured ones.
pub fn to_json(status: &Status, quota: &Quota) -> Value {
    let mut counters = serde_json::Map::new();
    for key in COUNTERS {
        if let Some(value) = status.get(key) {
            counters.insert((*key).to_string(), json!(value));
        }
    }
    json!({
        "model": status.text.get("model").cloned(),
        "counters": counters,
        "quota": {
            "percent_used": quota.percent_used,
            "quota_status": quota.quota_status,
            "resets_at": quota.resets_at,
            "tier": quota.tier,
            "topup_balance_credits": quota.topup_balance,
            "topup_total_credits": quota.topup_total,
            "topup_balance_tokens": quota
                .topup_balance
                .map(|b| b.saturating_mul(TOKENS_PER_CREDIT)),
            "topup_total_tokens": quota
                .topup_total
                .map(|t| t.saturating_mul(TOKENS_PER_CREDIT)),
            "tokens_per_credit": TOKENS_PER_CREDIT,
        },
        "derived": ["topup_balance_tokens", "topup_total_tokens"],
        "not_available": "the tier's weekly token cap is not exposed by the API",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPLY: &str = "model=Muse Spark total_input_tokens=1126899520 \
total_output_tokens=9232970 session_input_tokens=9020282 session_output_tokens=6431 \
session_subagent_input_tokens=0 session_subagent_output_tokens=0 \
session_aggregate_input_tokens=9020282 session_aggregate_output_tokens=6431 \
context_used_tokens=72145 context_window_tokens=350000 context_used_percent=21 \
context_available_tokens=277855 context_available_percent=79 usage=on cost=unavailable";

    fn quota() -> Quota {
        Quota {
            percent_used: Some(64),
            quota_status: Some("SUFFICIENT".into()),
            resets_at: Some(1_795_043_657),
            tier: Some("Muse Free".into()),
            topup_balance: Some(80_000_000),
            topup_total: Some(80_000_000),
        }
    }

    #[test]
    fn parse_reads_every_counter_as_an_integer() {
        let status = parse_status(REPLY);
        for key in COUNTERS {
            assert!(status.get(key).is_some(), "{key} was not parsed");
        }
        assert_eq!(status.get("total_input_tokens"), Some(1_126_899_520));
        assert_eq!(status.get("context_window_tokens"), Some(350_000));
    }

    #[test]
    fn parse_keeps_a_model_name_that_contains_a_space() {
        let status = parse_status(REPLY);
        assert_eq!(status.text.get("model").map(String::as_str), Some("Muse Spark"));
    }

    #[test]
    fn parse_keeps_non_numeric_fields_as_text() {
        let status = parse_status(REPLY);
        assert_eq!(status.text.get("usage").map(String::as_str), Some("on"));
        assert_eq!(status.text.get("cost").map(String::as_str), Some("unavailable"));
    }

    #[test]
    fn parse_tolerates_garbage() {
        let status = parse_status("");
        assert!(status.numbers.is_empty());
        assert!(status.text.is_empty());
        let odd = parse_status("=5 first=a=b=c");
        // An empty key is skipped rather than stored under "".
        assert!(odd.numbers.get("").is_none());
        // Only the first '=' splits, so a value may contain its own.
        assert_eq!(odd.text.get("first").map(String::as_str), Some("a=b=c"));
    }

    #[test]
    fn summary_prints_the_exact_counters_grouped() {
        let text = summarize(&parse_status(REPLY), &quota());
        assert!(text.contains("1,126,899,520"), "{text}");
        assert!(text.contains("72,145"), "{text}");
        assert!(text.contains("350,000"), "{text}");
        assert!(text.contains("21%"), "{text}");
    }

    #[test]
    fn summary_states_the_derivation_for_topup() {
        let text = summarize(&parse_status(REPLY), &quota());
        assert!(text.contains("2,000,000,000"), "derived tokens: {text}");
        assert!(text.contains("25x"), "basis must be shown: {text}");
    }

    #[test]
    fn summary_states_that_the_weekly_cap_is_unknown() {
        let text = summarize(&parse_status(REPLY), &quota());
        assert!(text.contains("no token equivalent"), "{text}");
    }

    #[test]
    fn summary_prints_dashes_when_the_command_said_nothing() {
        let text = summarize(&parse_status(""), &Quota {
            percent_used: None,
            quota_status: None,
            resets_at: None,
            tier: None,
            topup_balance: None,
            topup_total: None,
        });
        assert_eq!(text.lines().count(), 10, "every line stays present: {text}");
        assert!(text.contains("lifetime        in -  out -"), "{text}");
        assert!(text.contains("topup           -"), "{text}");
    }

    #[test]
    fn json_separates_measured_from_derived() {
        let value = to_json(&parse_status(REPLY), &quota());
        assert_eq!(value["counters"]["total_input_tokens"], 1_126_899_520);
        assert_eq!(value["quota"]["topup_balance_tokens"], 2_000_000_000);
        assert_eq!(value["derived"][0], "topup_balance_tokens");
        assert!(value["not_available"].as_str().unwrap().contains("weekly"));
    }

    #[test]
    fn json_omits_counters_the_command_did_not_report() {
        let value = to_json(&parse_status("model=X context_used_tokens=10"), &quota());
        assert_eq!(value["counters"]["context_used_tokens"], 10);
        assert!(value["counters"].get("total_input_tokens").is_none());
    }

    #[test]
    fn the_credit_ratio_matches_every_pack_in_the_catalog() {
        // 20M→500M, 80M→2B, 200M→5B, 480M→12B.
        for (credits, tokens) in [
            (20_000_000i64, 500_000_000i64),
            (80_000_000, 2_000_000_000),
            (200_000_000, 5_000_000_000),
            (480_000_000, 12_000_000_000),
        ] {
            assert_eq!(tokens / credits, TOKENS_PER_CREDIT);
        }
    }
}
