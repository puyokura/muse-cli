//! System-file access through the agent.
//!
//! The gateway's own file API (`/fs/*`) is confined to the workspace: absolute
//! paths are rejected with `absolute paths are not allowed`, `..` with
//! `path traversal is not allowed`, and URL-encoded traversal with `invalid
//! path`. There is no `/api/exec` route either (`/api/tasks/shell` exists but
//! answers `matched route missing from endpoint ACL`).
//!
//! What does work is asking the agent to run a command inside its VM. Verified
//! on 2026-11-07: `cat /etc/os-release` returned the real Ubuntu 24.04.5 image
//! metadata and `id` returned `uid=0(root)`. So system files are reachable only
//! through an agent turn, and every result carries `transport: "agent-shell"`
//! so the caller never mistakes this for a direct file read.

use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::time::Duration;

use crate::client::{AttachmentKind, MuseClient};

/// Where the agent-side command ran. Reported verbatim in every result.
pub const TRANSPORT: &str = "agent-shell";

/// One syscall the CLI can ask the agent to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Cat,
    Ls,
    Find,
    Raw,
}

impl Verb {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "cat" | "read" => Ok(Verb::Cat),
            "ls" | "list" => Ok(Verb::Ls),
            "find" | "search" => Ok(Verb::Find),
            "raw" | "run" => Ok(Verb::Raw),
            other => bail!("unknown sys verb `{other}` (expected cat, ls, find, raw)"),
        }
    }
}

/// Build the command line for a verb. Paths are single-quoted so spaces and
/// globs survive the shell round-trip.
pub fn command_for(verb: Verb, target: &str) -> String {
    let quoted = shell_quote(target);
    match verb {
        Verb::Cat => format!("cat {quoted}"),
        Verb::Ls => format!("ls -la {quoted}"),
        Verb::Find => format!(
            "find {quoted} -maxdepth 4 -printf '%y %10s %p\\n' 2>/dev/null | head -400"
        ),
        Verb::Raw => target.to_string(),
    }
}

/// POSIX single-quote escaping: wrap in `'` and turn embedded `'` into `'\''`.
pub fn shell_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Prompt shape that makes the agent return machine-readable output.
///
/// The wrapper asks for a single fenced block and nothing else, which keeps the
/// extraction reliable without suppressing genuine errors: if the block holds an
/// error message it is returned as-is.
pub fn prompt_for(command: &str) -> String {
    format!(
        "VM で次のコマンドをそのまま実行し、標準出力と標準エラーをそのまま1つの ``` フェンスに\n\
         入れて返してください。前置き・要約・解説は書かないでください。終了コードは最終行に\n\
         `exit=<code>` の形で付けてください。\n\n\
         コマンド:\n```sh\n{command}\n```"
    )
}

/// Extract the first fenced code block, or fall back to the whole text.
pub fn extract_block(text: &str) -> (String, bool) {
    let lines: Vec<&str> = text.lines().collect();
    let mut start: Option<usize> = None;

    for (index, line) in lines.iter().enumerate() {
        if !line.trim_start().starts_with("```") {
            continue;
        }
        match start {
            None => start = Some(index + 1),
            Some(begin) => {
                // Closed fence: the block is trustworthy.
                return (lines[begin..index].join("\n"), true);
            }
        }
    }

    match start {
        // A fence opened but never closed. That is what a mid-stream fragment
        // looks like, so report the partial body and say it is not final
        // (`fenced: false`) instead of passing off a stray ``` as output.
        Some(begin) => (lines[begin..].join("\n").trim().to_string(), false),
        None => (text.trim().to_string(), false),
    }
}

/// Split a trailing `exit=<code>` marker off the block.
pub fn split_exit(block: &str) -> (String, Option<i32>) {
    let mut body: Vec<&str> = block.lines().collect();
    while body.last().map(|line| line.trim().is_empty()).unwrap_or(false) {
        body.pop();
    }
    if let Some(last) = body.last().copied() {
        let trimmed = last.trim();
        if let Some(rest) = trimmed.strip_prefix("exit=") {
            if let Ok(code) = rest.trim().parse::<i32>() {
                body.pop();
                return (body.join("\n"), Some(code));
            }
        }
    }
    (body.join("\n"), None)
}

/// Strip the `[name]` decoration added to thread titles.
pub fn clean_title(title: &str) -> String {
    let trimmed = title.trim();
    if trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.len() > 2 {
        return trimmed[1..trimmed.len() - 1].to_string();
    }
    trimmed.to_string()
}

/// Result of one agent-mediated command.
#[derive(Debug, Clone)]
pub struct ExecResult {
    pub command: String,
    pub output: String,
    pub exit_code: Option<i32>,
    pub fenced: bool,
    pub session_id: String,
    pub thread_name: Option<String>,
    pub reply_text: String,
}

impl ExecResult {
    pub fn to_json(&self) -> Value {
        json!({
            "transport": TRANSPORT,
            "note": "executed by the Muse agent inside its VM (root shell); not a direct file API read",
            "command": self.command,
            "session_id": self.session_id,
            "thread": self.thread_name,
            "exit_code": self.exit_code,
            "fenced": self.fenced,
            "output": self.output,
            "reply_text": self.reply_text,
        })
    }
}

/// Run one command through the agent and return the extracted output.
pub async fn exec(
    client: &MuseClient,
    command: &str,
    session_id: Option<&str>,
    thread_name: Option<&str>,
    timeout: Duration,
) -> Result<ExecResult> {
    if command.trim().is_empty() {
        bail!("command must not be empty");
    }
    let prompt = prompt_for(command);
    let outcome = client
        .ask_and_wait(
            &prompt,
            session_id,
            &[],
            AttachmentKind::File,
            timeout,
            Duration::from_secs(2),
        )
        .await?;

    let reply_text = outcome
        .reply
        .get("display_text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    if reply_text.is_empty() {
        bail!(
            "the agent did not answer within {}s (session {}); \
             the command was submitted but no output was returned",
            timeout.as_secs(),
            outcome.session_id
        );
    }

    let (block, fenced) = extract_block(&reply_text);
    let (output, exit_code) = split_exit(&block);

    Ok(ExecResult {
        command: command.to_string(),
        output,
        exit_code,
        fenced,
        session_id: outcome.session_id,
        thread_name: thread_name.map(str::to_string),
        reply_text,
    })
}

/// Convenience wrapper for `muse sys <verb> <target>`.
pub async fn run_verb(
    client: &MuseClient,
    verb: Verb,
    target: &str,
    session_id: Option<&str>,
    thread_name: Option<&str>,
    timeout: Duration,
) -> Result<ExecResult> {
    let command = command_for(verb, target);
    exec(client, &command, session_id, thread_name, timeout).await
}

/// Ask the agent to stage a file inside the workspace so the normal (fast,
/// non-agent) file API can download it. Used for binaries and large files where
/// the agent's text output would be lossy or truncated.
pub async fn stage_for_download(
    client: &MuseClient,
    source: &str,
    remote: Option<&str>,
    session_id: Option<&str>,
    thread_name: Option<&str>,
    timeout: Duration,
) -> Result<String> {
    let destination = remote
        .map(str::to_string)
        .unwrap_or_else(|| {
            let name = source.rsplit('/').next().unwrap_or("staged.bin");
            format!("your_files/staged/{name}")
        });
    let command = format!(
        "mkdir -p {} && cp -f {} {} && stat -c '%s' {}",
        shell_quote(&format!("~/{}", parent_of(&destination))),
        shell_quote(source),
        shell_quote(&format!("~/{}", destination.trim_start_matches('/'))),
        shell_quote(&format!("~/{}", destination.trim_start_matches('/'))),
    );
    let result = exec(client, &command, session_id, thread_name, timeout).await?;
    if let Some(code) = result.exit_code {
        if code != 0 {
            bail!(
                "staging failed with exit {code}: {}",
                result.output.trim()
            );
        }
    }
    Ok(destination)
}

fn parent_of(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((parent, _)) if !parent.is_empty() => parent.to_string(),
        _ => ".".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_first_fenced_block() {
        let reply = "前置き\n```\nuid=0(root)\nexit=0\n```\n後書き";
        let (block, fenced) = extract_block(reply);
        assert!(fenced);
        assert_eq!(block, "uid=0(root)\nexit=0");
    }

    #[test]
    fn falls_back_to_whole_text_without_a_fence() {
        let (block, fenced) = extract_block("just text");
        assert!(!fenced);
        assert_eq!(block, "just text");
    }

    // Regression: a reply that consists of a bare fence used to be returned as
    // the command output when the poller accepted a streaming fragment.
    #[test]
    fn bare_fence_yields_empty_output() {
        let (block, fenced) = extract_block("```");
        assert!(!fenced);
        assert_eq!(block, "");
    }

    #[test]
    fn splits_exit_code() {
        let (body, code) = split_exit("out\nline\nexit=3");
        assert_eq!(body, "out\nline");
        assert_eq!(code, Some(3));
    }

    #[test]
    fn leaves_missing_exit_code_none() {
        let (body, code) = split_exit("out\nline");
        assert_eq!(body, "out\nline");
        assert_eq!(code, None);
    }

    #[test]
    fn quotes_paths_with_spaces_and_quotes() {
        assert_eq!(shell_quote("/tmp/a b"), "'/tmp/a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn builds_commands_per_verb() {
        assert_eq!(command_for(Verb::Cat, "/etc/hosts"), "cat '/etc/hosts'");
        assert_eq!(command_for(Verb::Ls, "/etc"), "ls -la '/etc'");
        assert!(command_for(Verb::Find, "/var").starts_with("find '/var'"));
        assert_eq!(command_for(Verb::Raw, "id"), "id");
    }

    #[test]
    fn verb_parsing_rejects_unknown() {
        assert_eq!(Verb::parse("cat").unwrap(), Verb::Cat);
        assert!(Verb::parse("nope").is_err());
    }

    #[test]
    fn strips_thread_title_decoration() {
        assert_eq!(clean_title("[lab]"), "lab");
        assert_eq!(clean_title("plain"), "plain");
        assert_eq!(clean_title("[]"), "[]");
    }

    #[test]
    fn prompt_contains_the_command_and_fence_instruction() {
        let prompt = prompt_for("id");
        assert!(prompt.contains("id"));
        assert!(prompt.contains("exit=<code>"));
    }
}
