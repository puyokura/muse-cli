//! Argument parsing and output rendering for the `muse` CLI.
//!
//! Deliberately hand-written: the project keeps its dependency list small, and
//! the grammar is a flat `command [subcommand] [positional...] [--flag [value]]`
//! shape that never needs a parser generator.
//!
//! Encoding note: values arrive through `argv`, which Windows passes as UTF-16
//! and Rust decodes losslessly. Piping JSON through stdin is only used when the
//! caller explicitly writes `--json -`, because Windows PowerShell 5.1 defaults
//! `$OutputEncoding` to us-ascii and silently replaces non-ASCII with `?` on the
//! way into a native process.

use anyhow::{bail, Result};
use serde_json::Value;
use std::collections::BTreeMap;

/// Parsed command line: positionals plus flags.
/// Switches that never take a value.
///
/// The parser cannot tell `--follow` (a bare switch) from `--thread lab` (a flag
/// with a value) by shape alone, so this list is authoritative. Without it,
/// `muse chat --no-wait "prompt"` consumed the prompt as the value of
/// `--no-wait` and the command then reported "needs a prompt".
const BOOLEAN_SWITCHES: [&str; 10] = [
    "confirm",
    "csrf",
    "follow",
    "json",
    "no-wait",
    "recursive",
    "all",
    "base64",
    "help",
    "json-stdin",
];

pub fn is_boolean_switch(name: &str) -> bool {
    BOOLEAN_SWITCHES.contains(&name)
}

pub struct Args {
    pub positionals: Vec<String>,
    flags: BTreeMap<String, Vec<String>>,
    switches: Vec<String>,
}

impl Args {
    pub fn parse(raw: &[String]) -> Result<Self> {
        let mut positionals = Vec::new();
        let mut flags: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut switches = Vec::new();
        let mut index = 0usize;

        while index < raw.len() {
            let token = &raw[index];
            if let Some(rest) = token.strip_prefix("--") {
                if rest.is_empty() {
                    bail!("empty flag `--`");
                }
                // `--flag=value` form.
                if let Some((name, value)) = rest.split_once('=') {
                    flags
                        .entry(name.to_string())
                        .or_default()
                        .push(value.to_string());
                    index += 1;
                    continue;
                }
                // A known switch never consumes the next token; otherwise a
                // positional that follows it would be swallowed.
                let takes_value = if is_boolean_switch(rest) {
                    false
                } else {
                    match raw.get(index + 1) {
                        Some(next) => !next.starts_with("--"),
                        None => false,
                    }
                };
                if takes_value {
                    flags
                        .entry(rest.to_string())
                        .or_default()
                        .push(raw[index + 1].clone());
                    index += 2;
                } else {
                    switches.push(rest.to_string());
                    index += 1;
                }
                continue;
            }
            if let Some(rest) = token.strip_prefix('-') {
                if rest.len() == 1 {
                    switches.push(rest.to_string());
                    index += 1;
                    continue;
                }
            }
            positionals.push(token.clone());
            index += 1;
        }

        Ok(Self {
            positionals,
            flags,
            switches,
        })
    }

    /// Every flag and switch the caller supplied, for validation.
    pub fn supplied(&self) -> Vec<String> {
        let mut names: Vec<String> = self.flags.keys().cloned().collect();
        names.extend(self.switches.iter().cloned());
        names.sort();
        names.dedup();
        names
    }

    pub fn has(&self, name: &str) -> bool {
        self.switches.iter().any(|flag| flag == name) || self.flags.contains_key(name)
    }

    pub fn flag(&self, name: &str) -> Option<&str> {
        self.flags
            .get(name)
            .and_then(|values| values.last())
            .map(String::as_str)
    }

    pub fn flag_all(&self, name: &str) -> Vec<String> {
        self.flags.get(name).cloned().unwrap_or_default()
    }

    pub fn flag_u64(&self, name: &str) -> Result<Option<u64>> {
        match self.flag(name) {
            None => Ok(None),
            Some(raw) => raw
                .parse::<u64>()
                .map(Some)
                .map_err(|_| anyhow::anyhow!("--{name} expects a non-negative integer, got `{raw}`")),
        }
    }


    /// Positional by index with a helpful error instead of a silent default.
    pub fn positional(&self, index: usize, label: &str) -> Result<String> {
        self.positionals
            .get(index)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing required argument <{label}>"))
    }

    pub fn positional_opt(&self, index: usize) -> Option<String> {
        self.positionals.get(index).cloned()
    }

    /// Join every positional from `index` onward with spaces. Used by `chat`
    /// and `ask` so a prompt does not need quoting gymnastics.
    pub fn rest_joined(&self, index: usize) -> Option<String> {
        if self.positionals.len() <= index {
            return None;
        }
        Some(self.positionals[index..].join(" "))
    }
}

/// Pretty-print a JSON value, keeping field order stable for diffs.
pub fn render(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}


/// Truncate for table display only. Callers that must not lose bytes print the
/// raw field separately; this helper is for scan-friendly columns.
pub fn ellipsis(text: &str, width: usize) -> String {
    let flattened: String = text
        .chars()
        .map(|ch| if ch == '\n' || ch == '\r' { ' ' } else { ch })
        .collect();
    let trimmed = flattened.trim();
    let count = trimmed.chars().count();
    if count <= width {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(width.saturating_sub(1)).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(input: &[&str]) -> Args {
        let owned: Vec<String> = input.iter().map(|value| value.to_string()).collect();
        Args::parse(&owned).expect("parse")
    }

    #[test]
    fn parses_flags_and_switch() {
        let parsed = args(&["ask", "hello world", "--thread", "lab", "--confirm"]);
        assert_eq!(parsed.positionals, vec!["ask", "hello world"]);
        assert_eq!(parsed.flag("thread"), Some("lab"));
        assert!(parsed.has("confirm"));
    }

    #[test]
    fn supports_equals_form() {
        let parsed = args(&["fs", "cat", "--offset=128", "file.md"]);
        assert_eq!(parsed.flag("offset"), Some("128"));
        assert_eq!(parsed.positional(2, "path").unwrap(), "file.md");
    }

    #[test]
    fn joins_remaining_positionals() {
        let parsed = args(&["chat", "run", "the", "tests", "--thread", "lab"]);
        assert_eq!(
            parsed.rest_joined(1).as_deref(),
            Some("run the tests")
        );
    }

    #[test]
    fn keeps_non_ascii_intact() {
        let parsed = args(&["ask", "日本語のプロンプト", "--thread", "実験"]);
        assert_eq!(parsed.positional(1, "prompt").unwrap(), "日本語のプロンプト");
        assert_eq!(parsed.flag("thread"), Some("実験"));
    }

    // Regression: `--no-wait` used to swallow the prompt that followed it, so
    // `muse chat --no-wait "hello"` failed with "needs a prompt".
    #[test]
    fn boolean_switch_does_not_consume_the_next_positional() {
        let parsed = args(&["chat", "--no-wait", "hello world"]);
        assert!(parsed.has("no-wait"));
        assert_eq!(
            parsed.rest_joined(1).as_deref(),
            Some("hello world")
        );
    }

    #[test]
    fn every_declared_switch_is_boolean() {
        for name in BOOLEAN_SWITCHES {
            assert!(is_boolean_switch(name));
        }
        assert!(!is_boolean_switch("thread"));
        assert!(!is_boolean_switch("path"));
    }

    #[test]
    fn supplied_lists_switches_and_flags() {
        let parsed = args(&["history", "--limit", "5", "--follow", "--session", "abc"]);
        let names = parsed.supplied();
        assert!(names.contains(&"limit".to_string()));
        assert!(names.contains(&"follow".to_string()));
        assert!(names.contains(&"session".to_string()));
        // Positionals are not flags.
        assert!(!names.contains(&"history".to_string()));
    }

    #[test]
    fn ellipsis_flattens_newlines() {
        assert_eq!(ellipsis("a\nb", 10), "a b");
        assert!(ellipsis("abcdefghij", 5).ends_with('…'));
    }
}
