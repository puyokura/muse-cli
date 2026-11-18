//! `muse` — Meta Muse (Hatch) from the command line, with an MCP mode.
//!
//! The CLI is the primary interface; `muse mcp` serves the same capability set
//! over the Model Context Protocol on stdio for agent hosts.
//!
//! Everything the CLI needs is reachable through the encrypted Hatch gateway
//! (`wss://<vm>.metaaivm.com/v1/noise`, Noise XX + protobuf) plus two HTTP
//! surfaces: `muse.ai/api/*` (cookies + CSRF) for login and gate credentials,
//! and `hatch-api.meta.ai/hatch/*` (abra bearer) for account data.

mod cli;
mod client;
mod doctor;
mod http;
mod mcp;
mod noise;
mod quota;
mod relay;
mod slash;
mod sysfs;
mod thread;
mod tools;
mod transport;
mod usage;
mod wire;

use anyhow::{anyhow, bail, Context, Result};
use cli::Args;
use client::MuseClient;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const DEFAULT_REPLY_TIMEOUT: u64 = 180;
const DEFAULT_EXEC_TIMEOUT: u64 = 240;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("MUSE_MCP_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to start the async runtime")?;
    runtime.block_on(run())
}

async fn run() -> Result<()> {
    let raw: Vec<String> = std::env::args().skip(1).collect();

    // `--state` is global: pull it out before command parsing.
    let mut state_path = http::default_state_path();
    let mut filtered: Vec<String> = Vec::with_capacity(raw.len());
    let mut index = 0usize;
    while index < raw.len() {
        if raw[index] == "--state" {
            let value = raw
                .get(index + 1)
                .ok_or_else(|| anyhow!("--state requires a path"))?;
            state_path = PathBuf::from(value);
            index += 2;
            continue;
        }
        if let Some(value) = raw[index].strip_prefix("--state=") {
            state_path = PathBuf::from(value);
            index += 1;
            continue;
        }
        filtered.push(raw[index].clone());
        index += 1;
    }

    if filtered.is_empty() || filtered[0] == "help" || filtered[0] == "--help" || filtered[0] == "-h" {
        print_help();
        return Ok(());
    }
    if filtered[0] == "version" || filtered[0] == "--version" || filtered[0] == "-V" {
        println!("muse {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let args = Args::parse(&filtered)?;
    let client = Arc::new(MuseClient::new(state_path)?);

    let command = args.positionals.first().cloned().unwrap_or_default();
    reject_unknown_flags(&command, &args)?;
    match command.as_str() {
        "mcp" | "serve" | "stdio" => mcp::Server::new(client).serve_stdio().await,
        "login" => cmd_login(&client, &args).await,
        "logout" => cmd_logout(&client).await,
        "status" => cmd_status(&client).await,
        "ping" => cmd_ping(&client).await,
        "model" => cmd_model(&client).await,
        "threads" => cmd_threads(&client, &args).await,
        "thread" => cmd_thread(&client, &args).await,
        "chat" | "ask" => cmd_chat(&client, &args).await,
        "doctor" => cmd_doctor(&client, &args).await,
        "relay" => cmd_relay(&client, &args).await,
        "history" => cmd_history(&client, &args).await,
        "cancel" => cmd_cancel(&client, &args).await,
        "exec" | "run" => cmd_exec(&client, &args).await,
        "sys" => cmd_sys(&client, &args).await,
        "fs" => cmd_fs(&client, &args).await,
        "goals" => cmd_goals(&client, &args).await,
        "settings" => cmd_settings(&client, &args).await,
        "quota" => cmd_quota(&client, &args).await,
        "slash" => cmd_slash(&client, &args).await,
        "usage" => cmd_usage(&client, &args).await,
        "raw" => cmd_raw(&client, &args).await,
        "rpc" => cmd_rpc(&client, &args).await,
        "service" => cmd_service(&client, &args).await,
        "approvals" => cmd_approvals(&client, &args).await,
        "web" => cmd_web(&client, &args).await,
        "tools" => print_tools(),
        "call" => cmd_call(&client, &args).await,
        other => bail!("unknown command `{other}`; run `muse help`"),
    }
}

/// Flags every command understands.
const GLOBAL_FLAGS: [&str; 3] = ["state", "thread", "session"];

/// Refuse a flag the chosen command does not read.
///
/// Without this, `muse history --muse-session <id>` silently returned the
/// primary chat: the typo and the unsupported flag were both discarded, and the
/// caller saw data that looked like an answer. A rejected flag is cheap; a
/// wrong conversation returned as if it were the right one is not.
fn reject_unknown_flags(command: &str, args: &Args) -> Result<()> {
    let known: &[&str] = match command {
        "status" | "ping" | "model" | "logout" | "tools" => &[],
        "login" => &["start", "finish", "region"],
        "threads" => &["all", "json"],
        "thread" => &["confirm", "json"],
        "chat" | "ask" => &["no-wait", "image", "file", "timeout", "json"],
        "doctor" => &["live", "json"],
        "quota" => &["json"],
        "slash" => &["list", "json", "timeout"],
        "usage" => &["json", "timeout"],
        "relay" => &[
            "muse-session", "dsh-session", "request-id", "image", "file", "timeout", "poll",
        ],
        "history" => &["limit", "follow", "interval", "until-idle", "count", "json"],
        "cancel" => &[],
        "exec" | "run" => &["timeout"],
        "sys" => &["timeout", "save", "out"],
        "fs" => &[
            "recursive", "confirm", "offset", "len", "base64", "ext", "preset",
        ],
        "goals" => &["json", "title", "summary", "description", "emoji"],
        "settings" => &["json"],
        "raw" => &["method", "out", "timeout", "json", "json-file", "set"],
        "rpc" | "service" => &["json", "json-file", "set", "confirm"],
        "approvals" => &["json", "confirm", "limit"],
        "web" => &["csrf", "json", "json-file", "set"],
        "call" => &["json", "json-file", "set"],
        _ => return Ok(()),
    };
    for name in args.supplied() {
        if GLOBAL_FLAGS.contains(&name.as_str()) || known.contains(&name.as_str()) {
            continue;
        }
        let mut all: Vec<&str> = GLOBAL_FLAGS.to_vec();
        all.extend_from_slice(known);
        all.sort();
        bail!(
            "unknown flag `--{name}` for `muse {command}`; known flags: {}",
            all.join(", ")
        );
    }
    Ok(())
}

fn print_help() {
    println!(
        "\
muse {version} — Meta Muse (Hatch) CLI, with an MCP server mode

USAGE
  muse <command> [subcommand] [args] [--flags]

CHAT
  muse chat [prompt...]              Send a prompt and print the reply
  muse chat --thread <name> [...]    Run inside a named sub-chat
  muse relay <prompt...>             Send with DSH<->Muse correlation headers
  muse chat --file <path> [...]      Attach a file (stored in the workspace)
  muse chat --image <path> [...]     Attach an image (viewed inline)
  muse doctor [--live] [--json]     Check state, auth, and the gateway
  muse quota [--json]              Subscription tier, usage %, reset time, top-up
  muse slash [--list] [CMD [args]] Documented /commands: status, compact, clear, usage
  muse usage [--json]              Exact token counters plus quota fields
  muse approvals [list|show|decide]  List or decide agent approvals
  muse ask <prompt...>               Alias of chat
  muse threads [--all]               List primary chat and sub-chats
  muse thread new <name> [prompt...] Create a sub-chat
  muse thread <action> <name>        rename|pin|unpin|archive|unarchive|delete
  muse history [--thread N] [--limit N]
  muse cancel <session-id>

VM ACCESS (runs through the agent, not a file API)
  muse exec \"<command>\" [--thread N] [--timeout S]
  muse sys cat|ls|find <path> [--thread N]
  muse sys raw \"<command>\" [--thread N]

WORKSPACE FILES (direct gateway file API)
  muse fs ls [path]             muse fs stat <path>
  muse fs cat <path> [--offset N] [--len N] [--base64]
  muse fs get <remote> <local>  muse fs put <local> <remote>
  muse fs mkdir <path> [--recursive]
  muse fs mv <from> <to>        muse fs rm <path> [--recursive] [--confirm]
  muse fs search <query> [--ext md,txt]
  muse fs library [--preset recents|media|artifacts]

ACCOUNT
  muse status | ping | model | settings read
  muse settings set-voice <voice-id>
  muse goals list|get|create|update|delete
  muse web <METHOD> /api/... [--json JSON] [--json-file F] [--csrf]

LOW LEVEL
  muse rpc <METHOD> <path> [--json JSON] [--json-file F] [--confirm]
  muse tools            List MCP tool names
  muse call <tool> [--json JSON]
  muse mcp              Serve MCP over stdio
  muse login <contact> [region]     Interactive OTP login
  muse login --start <contact>      Send OTP only
  muse login --finish <code>        Complete OTP

RELAY HEADERS
  --muse-session ID   Muse conversation the prompt is delivered to
  --dsh-session ID    DSH session that issued this turn
  --request-id ID     Correlation id for this single turn
  Env fallback: MUSE_SESSION_ID / DSH_SESSION_ID / REQUEST_ID (flags win).
  Values must be non-empty, <= 256 chars, and free of control characters.

JSON INPUT
  --set key=value        First choice: no quote characters, so no shell can
                         mangle it. Repeatable. true/false/null and integers
                         are typed; a,b,c becomes an array.
  --json-file PATH       Second choice: exact bytes from a file (UTF-8).
  --json JSON            Only when the shell preserves quotes (bash, cmd).
  --json -               Read UTF-8 JSON from stdin.

  Windows PowerShell 5.1 mangles both routes: it strips double quotes even
  inside single quotes before a native process sees the argument:
      single-quoted JSON  ->  quotes stripped, so the JSON is invalid
  and it defaults $OutputEncoding to us-ascii, which rewrites non-ASCII to '?'
  on the way into a pipe. Use --set or --json-file there.

GLOBAL
  --state PATH   Session file (default: OS config dir/muse-mcp/session.json)
  --thread NAME  Target a named sub-chat

ENVIRONMENT
  MUSE_MCP_STATE   Override the session file path
  MUSE_MCP_LOG     tracing filter, e.g. muse_mcp=debug
",
        version = env!("CARGO_PKG_VERSION")
    );
}

fn print_tools() -> Result<()> {
    let tools = tools::definitions();
    for tool in tools {
        if let Some(name) = tool.get("name").and_then(Value::as_str) {
            println!("{name}");
        }
    }
    Ok(())
}

fn reply_timeout(args: &Args) -> Result<Duration> {
    Ok(Duration::from_secs(
        args.flag_u64("timeout")?
            .unwrap_or(DEFAULT_REPLY_TIMEOUT)
            .max(5),
    ))
}

fn exec_timeout(args: &Args) -> Result<Duration> {
    Ok(Duration::from_secs(
        args.flag_u64("timeout")?
            .unwrap_or(DEFAULT_EXEC_TIMEOUT)
            .max(10),
    ))
}

fn thread_name(args: &Args) -> Result<Option<String>> {
    thread::normalise(args.flag("thread"))
}

/// A raw session id given with `--session`.
///
/// Relay work targets session ids that were handed to us; those are not named
/// threads, so they cannot go through `--thread`. Every command that reads or
/// writes a conversation accepts this flag, and a value that is not a UUID is
/// refused rather than looked up.
fn session_flag(args: &Args) -> Result<Option<String>> {
    let Some(raw) = args
        .flag("session")
        .or_else(|| args.flag("muse-session"))
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    if !crate::relay::is_uuid(raw) {
        bail!("--session expects a lower-case session UUID; got `{raw}`");
    }
    Ok(Some(raw.to_string()))
}

/// Resolve `--thread` to a session id, refusing to invent one.
///
/// `--session` takes precedence: it names a conversation directly, so there is
/// nothing to look up.
async fn resolve_thread(
    client: &MuseClient,
    args: &Args,
) -> Result<(Option<String>, Option<String>)> {
    if let Some(session_id) = session_flag(args)? {
        return Ok((Some(session_id), None));
    }
    let Some(name) = thread_name(args)? else {
        return Ok((None, None));
    };
    let resolved = thread::resolve(client, Some(&name)).await?;
    if !resolved.exists {
        return Err(thread::missing_error(&name));
    }
    // Show the server-side title (which carries the `[name]` decoration) when
    // it exists, so the output matches what the app displays.
    let label = resolved.label();
    Ok((resolved.session_id, Some(label)))
}

async fn cmd_login(client: &Arc<MuseClient>, args: &Args) -> Result<()> {
    let contact = args.flag("start").or_else(|| args.flag("finish"));
    let mut http = client.http_mut().await;

    if let Some(code) = args.flag("finish") {
        http.confirm_otp(code).await?;
        let saved = http.save_account().await?;
        let check = http.auth_check().await?;
        println!(
            "{}",
            tools::render_public(&json!({
                "signed_in": check.get("ok").and_then(Value::as_bool).unwrap_or(false),
                "save_account": saved,
                "auth_check": check,
            }))
        );
        return Ok(());
    }

    let Some(contact) = contact else {
        // Interactive form: `muse login <contact> [region]`
        let contact = args.positional(1, "contact")?;
        let region = args.positional_opt(2).unwrap_or_else(|| "US".to_string());
        http.restart_login().await?;
        let sent = http.send_otp(&contact, &region).await?;
        println!("{}", tools::render_public(&sent));
        print!("Enter the code: ");
        use std::io::Write;
        std::io::stdout().flush()?;
        let mut code = String::new();
        std::io::stdin().read_line(&mut code)?;
        let code = code.trim().to_string();
        if code.is_empty() {
            bail!("no code entered");
        }
        http.confirm_otp(&code).await?;
        http.save_account().await?;
        let check = http.auth_check().await?;
        println!(
            "{}",
            tools::render_public(&json!({
                "signed_in": check.get("ok").and_then(Value::as_bool).unwrap_or(false),
                "auth_check": check,
            }))
        );
        return Ok(());
    };

    let region = args
        .positional_opt(2)
        .or_else(|| args.flag("region").map(str::to_string))
        .unwrap_or_else(|| "US".to_string());
    http.restart_login().await?;
    let sent = http.send_otp(&contact, &region).await?;
    println!("{}", tools::render_public(&sent));
    Ok(())
}

async fn cmd_logout(client: &Arc<MuseClient>) -> Result<()> {
    client.disconnect().await;
    let mut http = client.http_mut().await;
    let result = http.logout().await?;
    println!("{}", tools::render_public(&json!({ "signed_out": true, "result": result })));
    Ok(())
}

async fn cmd_status(client: &Arc<MuseClient>) -> Result<()> {
    let check = {
        let mut http = client.http_mut().await;
        http.auth_check().await?
    };
    println!(
        "{}",
        tools::render_public(&json!({
            "state_path": client.state_path().display().to_string(),
            "signed_in": check.get("ok").and_then(Value::as_bool).unwrap_or(false),
            "auth_check": check,
        }))
    );
    Ok(())
}

async fn cmd_ping(client: &Arc<MuseClient>) -> Result<()> {
    println!("{}", cli::render(&client.ping().await?));
    Ok(())
}

async fn cmd_model(client: &Arc<MuseClient>) -> Result<()> {
    println!("{}", cli::render(&client.model().await?));
    Ok(())
}

async fn cmd_threads(client: &Arc<MuseClient>, args: &Args) -> Result<()> {
    let sessions = client.sessions().await?;
    let rows = thread::parse_sessions(&sessions);
    let names = thread::name_index(&rows);
    let mut summary = thread::summarise(&rows);
    if !args.has("all") {
        if let Some(object) = summary.as_object_mut() {
            object.remove("other");
        }
    }
    // Annotate derived names so `--thread` strings are discoverable from ids.
    if let Some(object) = summary.as_object_mut() {
        for key in ["primary", "threads", "other"] {
            if let Some(entries) = object.get_mut(key).and_then(Value::as_array_mut) {
                for entry in entries.iter_mut() {
                    if let Some(id) = entry.get("session_id").and_then(Value::as_str) {
                        if let Some(name) = names.get(id) {
                            if let Some(map) = entry.as_object_mut() {
                                map.insert(
                                    "derived_name".to_string(),
                                    json!(sysfs::clean_title(name)),
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    if args.has("json") {
        println!("{}", cli::render(&summary));
        return Ok(());
    }

    // Compact table for humans; the JSON form above stays lossless.
    let render_rows = |label: &str, entries: &Value| {
        let Some(items) = entries.as_array() else { return };
        for entry in items {
            let id = entry.get("session_id").and_then(Value::as_str).unwrap_or("-");
            let title = entry.get("title").and_then(Value::as_str).unwrap_or("");
            let derived = entry
                .get("derived_name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let flags = {
                let mut parts: Vec<&str> = Vec::new();
                if entry.get("archived").and_then(Value::as_bool).unwrap_or(false) {
                    parts.push("archived");
                }
                if entry.get("pinned").and_then(Value::as_bool).unwrap_or(false) {
                    parts.push("pinned");
                }
                parts.join(",")
            };
            let shown = if derived.is_empty() { title } else { derived };
            println!(
                "{label:<8} {short}  {shown}  {flags}",
                short = &id[..id.len().min(8)],
                shown = cli::ellipsis(shown, 48),
            );
        }
    };

    if let Some(primary) = summary.get("primary") {
        render_rows("primary", primary);
    }
    if let Some(threads) = summary.get("threads") {
        render_rows("thread", threads);
    }
    if args.has("all") {
        if let Some(other) = summary.get("other") {
            render_rows("other", other);
        }
    }
    Ok(())
}

async fn cmd_thread(client: &Arc<MuseClient>, args: &Args) -> Result<()> {
    let action = args.positional(1, "action")?;
    let name = args.positional(2, "name")?;
    thread::validate_name(&name)?;

    match action.as_str() {
        "new" | "create" => {
            let opening = args.rest_joined(3);
            let summary = thread::ensure(
                client,
                &name,
                opening.as_deref(),
                reply_timeout(args)?,
            )
            .await?;
            println!(
                "{}",
                cli::render(&json!({
                    "created": true,
                    "thread": name,
                    "session_id": summary.session_id,
                    "title": summary.title,
                    "hint": format!("use --thread {name} to target it"),
                }))
            );
            Ok(())
        }
        other => {
            let resolved = thread::resolve(client, Some(&name)).await?;
            if !resolved.exists {
                return Err(thread::missing_error(&name));
            }
            let session_id = resolved.session_id.expect("named thread has an id");
            let mut body = json!({ "session_id": session_id });
            let path = match other {
                "rename" => {
                    let title = args
                        .rest_joined(3)
                        .ok_or_else(|| anyhow!("muse thread rename <name> <new title>"))?;
                    body["title"] = json!(title);
                    "/api/session/rename"
                }
                "pin" => {
                    body["method"] = json!("/api/session/pin");
                    "/api/session/pin"
                }
                "unpin" => {
                    body["method"] = json!("/api/session/unpin");
                    "/api/session/unpin"
                }
                "archive" => {
                    body["method"] = json!("/api/session/archive");
                    "/api/session/archive"
                }
                "unarchive" => {
                    body["method"] = json!("/api/session/unarchive");
                    "/api/session/unarchive"
                }
                "delete" => {
                    if !args.has("confirm") {
                        bail!("deleting a thread is irreversible; add --confirm");
                    }
                    body["method"] = json!("/api/session/delete");
                    "/api/session/delete"
                }
                unknown => bail!(
                    "unknown thread action `{unknown}`; \
                     expected new|rename|pin|unpin|archive|unarchive|delete"
                ),
            };
            println!("{}", cli::render(&client.raw_rpc("POST", path, Some(body)).await?));
            Ok(())
        }
    }
}

/// Attachments and their delivery form from `--image` / `--file`.
///
/// The two flags are mutually exclusive on purpose: mixing them would ask for a
/// shape the gateway has no single answer for, and silently picking one form
/// for the other's files is worse than an error.
fn chat_attachments(args: &Args) -> Result<(Vec<PathBuf>, client::AttachmentKind)> {
    let images: Vec<PathBuf> = args
        .flag_all("image")
        .into_iter()
        .map(PathBuf::from)
        .collect();
    let files: Vec<PathBuf> = args
        .flag_all("file")
        .into_iter()
        .map(PathBuf::from)
        .collect();

    match (images.is_empty(), files.is_empty()) {
        (false, false) => bail!(
            "pass --image or --file, not both: they are delivered differently \
             (viewed inline vs stored in the workspace)"
        ),
        (true, _) => Ok((files, client::AttachmentKind::File)),
        (_, true) => Ok((images, client::AttachmentKind::Image)),
    }
}

/// Send a documented slash command, or list them.
///
/// A slash command is a chat turn whose text starts with `/name`; the gateway
/// interprets it. Reaching for a private endpoint instead would be guessing at
/// an API the app does not expose, and a guessed route that happens to 404 would
/// look like "unsupported" rather than "not investigated".
async fn cmd_slash(client: &Arc<MuseClient>, args: &Args) -> Result<()> {
    if args.has("list") || args.positional_opt(1).is_none() {
        println!("{}", slash::list_text());
        return Ok(());
    }
    let name = args.positional(1, "command")?;
    let rest: Vec<String> = args.positionals.iter().skip(2).cloned().collect();
    let line = slash::build(&name, &rest)?;
    // Warn before sending: an undocumented name is still sent, but the caller
    // should know the server was asked to interpret something we cannot vouch for.
    if slash::find(&name).is_none() {
        eprintln!(
            "note: `{name}` is not one of the documented commands ({}); sending anyway",
            slash::COMMANDS
                .iter()
                .map(|command| command.name)
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    let (session_id, thread) = resolve_thread(client, args).await?;
    let outcome = client
        .ask_and_wait(
            &line,
            session_id.as_deref(),
            &[],
            client::AttachmentKind::File,
            reply_timeout(args)?,
            Duration::from_secs(2),
        )
        .await?;
    let text = outcome
        .reply
        .get("display_text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    println!(
        "{}",
        cli::render(&json!({
            "sent": line,
            "thread": thread,
            "session_id": outcome.session_id,
            "sent_message_id": outcome.sent_message_id,
            "reply": text,
        }))
    );
    Ok(())
}

/// Exact token counters, read from the `/status` command plus the subscription
/// payload for the quota fields.
///
/// The subscription API exposes only a percentage, so the counters come from the
/// command the app itself uses. Both sources are reported separately because
/// they measure different things.
async fn cmd_usage(client: &Arc<MuseClient>, args: &Args) -> Result<()> {
    let (session_id, thread) = resolve_thread(client, args).await?;
    let outcome = client
        .ask_and_wait(
            "/status",
            session_id.as_deref(),
            &[],
            client::AttachmentKind::File,
            reply_timeout(args)?,
            Duration::from_secs(2),
        )
        .await?;
    let reply = outcome
        .reply
        .get("display_text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if reply.trim().is_empty() {
        bail!(
            "/status returned no text; the counters are unavailable, so there is \
             nothing to report rather than zeroes"
        );
    }
    let status = usage::parse_status(&reply);
    let quota = {
        let http = client.http_mut().await;
        let payload = http
            .stefi_get("/hatch/subscription?include=agreement,offers")
            .await?;
        usage::Quota::from_subscription(&payload)
    };
    if args.has("json") {
        println!("{}", cli::render(&usage::to_json(&status, &quota)));
    } else {
        println!("{}", usage::summarize(&status, &quota));
    }
    let _ = thread;
    Ok(())
}

/// Read the subscription payload and report the quota with its denominator.
///
/// The percentage alone is not the answer: it means nothing without the tier it
/// is measured against and the moment it resets. `--json` returns the raw
/// payload so a caller can read fields this summary does not name.
async fn cmd_quota(client: &Arc<MuseClient>, args: &Args) -> Result<()> {
    let http = client.http_mut().await;
    let payload = http
        .stefi_get("/hatch/subscription?include=agreement,offers")
        .await?;
    drop(http);
    if args.has("json") {
        println!("{}", cli::render(&payload));
    } else {
        println!("{}", quota::summarize(&payload));
    }
    Ok(())
}

/// Run the health checks and summarise each layer.
async fn cmd_doctor(client: &Arc<MuseClient>, args: &Args) -> Result<()> {
    let live = args.has("live");
    let report = doctor::diagnose(client, live).await?;
    if args.has("json") {
        println!("{}", cli::render(&report.to_json()));
    } else {
        println!("{}", report.to_text());
    }
    if report.failed() {
        // A non-zero exit is what a script checks; printing the reason above
        // is for the human. Both are needed.
        std::process::exit(1);
    }
    Ok(())
}


async fn cmd_chat(client: &Arc<MuseClient>, args: &Args) -> Result<()> {
    let prompt = args
        .rest_joined(1)
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| anyhow!("muse chat needs a prompt"))?;
    let (session_id, name) = resolve_thread(client, args).await?;
    let (attachments, kind) = chat_attachments(args)?;

    if args.has("no-wait") {
        let events = client
            .ask(
                &prompt,
                session_id.as_deref(),
                &attachments,
                kind,
                reply_timeout(args)?,
            )
            .await?;
        println!(
            "{}",
            cli::render(&json!({
                "thread": name,
                "session_id": session_id,
                "events": events,
                "note": "--no-wait: only the submission acknowledgement is reported",
            }))
        );
        return Ok(());
    }

    let outcome = client
        .ask_and_wait(
            &prompt,
            session_id.as_deref(),
            &attachments,
            kind,
            reply_timeout(args)?,
            Duration::from_secs(2),
        )
        .await?;
    let replied = !outcome.reply.is_null();
    println!(
        "{}",
        cli::render(&json!({
            "thread": name,
            "session_id": outcome.session_id,
            "sent_message_id": outcome.sent_message_id,
            "replied": replied,
            "reply": outcome.reply,
        }))
    );
    Ok(())
}

/// Send one prompt to Muse with the relay correlation headers attached and
/// wait for the reply.
///
/// This is the dedicated entry point for DSH <-> Muse turns: it always sends
/// `museSessionId`, `dshSessionId`, and `requestId` (by flag or environment) and
/// reports the triple back so the caller can log it against its own transcript.
async fn cmd_relay(client: &Arc<MuseClient>, args: &Args) -> Result<()> {
    let prompt = args
        .rest_joined(1)
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| anyhow!("muse relay needs a prompt"))?;

    let relay = relay_headers(args)?;
    // A named thread and an explicit destination must agree, or the block would
    // point at one conversation while the prompt lands in another.
    let (thread_session, name) = resolve_thread(client, args).await?;
    if let (Some(header_session), Some(thread_session)) =
        (&relay.muse_session_id, &thread_session)
    {
        if header_session != thread_session {
            bail!(
                "--muse-session ({header_session}) and the resolved thread ({thread_session}) point at different conversations"
            );
        }
    }

    let (attachments, kind) = chat_attachments(args)?;
    let envelope = relay::RelayEnvelope::resolve(
        relay.muse_session_id.as_deref().or(thread_session.as_deref()),
        relay.dsh_session_id.as_deref(),
        relay.request_id.as_deref(),
        &prompt,
    )?;
    let headers = envelope.to_headers()?;
    let started = std::time::Instant::now();

    let outcome = client
        .ask_and_wait_with_headers(
            &envelope.body,
            Some(&envelope.muse_session_id),
            &attachments,
            kind,
            reply_timeout(args)?,
            Duration::from_secs(args.flag_u64("poll")?.unwrap_or(2).max(1)),
            &headers,
        )
        .await;

    let (delivered, detail) = match &outcome {
        Ok(outcome) => (true, outcome.sent_message_id.clone()),
        Err(err) => (false, Some(err.to_string())),
    };
    relay::write_audit(
        client.state_path(),
        &json!({
            "at_ms": relay::now_ms(),
            "tool": "muse relay",
            "museSessionId": envelope.muse_session_id,
            "dshSessionId": envelope.dsh_session_id,
            "requestId": envelope.request_id,
            "delivered": delivered,
            "detail": detail,
        }),
    );

    let outcome = outcome?;

    let text = outcome
        .reply
        .get("display_text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    println!(
        "{}",
        cli::render(&json!({
            "relay": envelope.to_json(),
            "session_id": outcome.session_id,
            "sent_message_id": outcome.sent_message_id,
            "replied": !text.is_empty(),
            "text": text,
            "elapsed_ms": started.elapsed().as_millis() as u64,
            "thread": name,
        }))
    );
    Ok(())
}

/// Relay ids from flags, falling back to environment variables.
///
/// Environment fallback exists so a DSH turn can export its session id once and
/// not repeat it on every call.
fn relay_headers(args: &Args) -> Result<relay::RelayHeaders> {
    let pick = |flag: &str, env: &str| -> Option<String> {
        args.flag(flag)
            .map(str::to_string)
            .or_else(|| std::env::var(env).ok())
            .filter(|value| !value.trim().is_empty())
    };
    Ok(relay::RelayHeaders {
        muse_session_id: pick("muse-session", "MUSE_SESSION_ID"),
        dsh_session_id: pick("dsh-session", "DSH_SESSION_ID"),
        request_id: pick("request-id", "REQUEST_ID"),
    })
}

async fn cmd_history(client: &Arc<MuseClient>, args: &Args) -> Result<()> {
    let (session_id, name) = resolve_thread(client, args).await?;
    if args.has("follow") {
        return follow_history(client, session_id.as_deref(), name.as_deref(), args).await;
    }
    let limit = args.flag_u64("limit")?.unwrap_or(40) as u32;
    let history = client.history(session_id.as_deref(), limit).await?;
    println!(
        "{}",
        cli::render(&json!({
            "thread": name,
            "session_id": session_id,
            "history": history,
        }))
    );
    Ok(())
}

/// Poll history and print only events newer than the highest `seq` seen.
///
/// A subscription route exists (`/chat/subscribe`) but answers only after the
/// turn settles (>60 s), so polling is what actually streams. `--follow` therefore
/// means: emit each new event once, as JSON Lines, until interrupted or until
/// `--until-idle` seconds pass with no activity.
async fn follow_history(
    client: &Arc<MuseClient>,
    session_id: Option<&str>,
    name: Option<&str>,
    args: &Args,
) -> Result<()> {
    let interval = Duration::from_secs(args.flag_u64("interval")?.unwrap_or(2).max(1));
    let idle_limit = args.flag_u64("until-idle")?;
    let max_events = args.flag_u64("count")?;

    // Seed the cursor with what already exists so `--follow` shows only new
    // traffic instead of replaying the whole transcript.
    let baseline = client.history(session_id, 80).await?;
    let mut cursor = newest_seq(&baseline);
    let mut emitted = 0u64;
    let mut last_activity = std::time::Instant::now();

    eprintln!(
        "following {}{} from seq {} (interval {}s)",
        name.unwrap_or("primary"),
        session_id
            .map(|id| format!(" ({id})"))
            .unwrap_or_default(),
        cursor,
        interval.as_secs()
    );

    loop {
        tokio::time::sleep(interval).await;
        let history = match client.history(session_id, 80).await {
            Ok(history) => history,
            Err(err) => {
                eprintln!("history poll failed: {err}");
                continue;
            }
        };
        let events = history_events(&history);
        for event in events {
            let seq = event.get("seq").and_then(Value::as_i64).unwrap_or(0);
            if seq as u64 <= cursor {
                continue;
            }
            cursor = seq as u64;
            // JSON Lines: one event per line so the output can be piped into jq
            // without a growing array.
            println!("{}", serde_json::to_string(&event)?);
            emitted += 1;
            last_activity = std::time::Instant::now();
        }
        if let Some(max) = max_events {
            if emitted >= max {
                eprintln!("stopped after {emitted} events (--count)");
                return Ok(());
            }
        }
        if let Some(idle) = idle_limit {
            if last_activity.elapsed() >= Duration::from_secs(idle) {
                eprintln!("stopped after {idle}s idle");
                return Ok(());
            }
        }
    }
}

/// Highest `seq` in a history payload.
fn newest_seq(history: &Value) -> u64 {
    history_events(history)
        .iter()
        .filter_map(|event| event.get("seq").and_then(Value::as_i64))
        .max()
        .unwrap_or(0)
        .max(0) as u64
}

/// The `chat_events` array, tolerating a wrapped payload.
fn history_events<'a>(history: &'a Value) -> &'a [Value] {
    history
        .get("chat_events")
        .and_then(Value::as_array)
        .or_else(|| history.get("result")?.get("chat_events")?.as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

async fn cmd_cancel(client: &Arc<MuseClient>, args: &Args) -> Result<()> {
    let session_id = match args.positional_opt(1) {
        Some(id) => id,
        None => resolve_thread(client, args)
            .await?
            .0
            .ok_or_else(|| anyhow!("muse cancel needs a session id or --thread NAME"))?,
    };
    println!("{}", cli::render(&client.cancel_stream(&session_id).await?));
    Ok(())
}

async fn cmd_exec(client: &Arc<MuseClient>, args: &Args) -> Result<()> {
    let command = args
        .rest_joined(1)
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| anyhow!("muse exec needs a command"))?;
    let (session_id, name) = resolve_thread(client, args).await?;
    if name.is_none() {
        eprintln!(
            "note: running in the primary chat; pass --thread <name> to keep experiments out of it"
        );
    }
    let result = sysfs::exec(
        client,
        &command,
        session_id.as_deref(),
        name.as_deref(),
        exec_timeout(args)?,
    )
    .await?;
    println!("{}", cli::render(&result.to_json()));
    Ok(())
}

async fn cmd_sys(client: &Arc<MuseClient>, args: &Args) -> Result<()> {
    let verb_raw = args.positional(1, "cat|ls|find|raw")?;
    let target = args
        .rest_joined(2)
        .ok_or_else(|| anyhow!("muse sys {verb_raw} needs a path or command"))?;
    let verb = sysfs::Verb::parse(&verb_raw)?;
    let (session_id, name) = resolve_thread(client, args).await?;
    if name.is_none() {
        eprintln!(
            "note: running in the primary chat; pass --thread <name> to keep experiments out of it"
        );
    }
    let result = sysfs::run_verb(
        client,
        verb,
        &target,
        session_id.as_deref(),
        name.as_deref(),
        exec_timeout(args)?,
    )
    .await?;

    if let Some(remote) = args.flag("save") {
        // Stage inside the workspace, then pull through the fast file API.
        sysfs::stage_for_download(
            client,
            &target,
            Some(remote),
            session_id.as_deref(),
            name.as_deref(),
            exec_timeout(args)?,
        )
        .await?;
        let bytes = client.fs_download(remote).await?;
        let local = args
            .flag("out")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(remote.rsplit('/').next().unwrap_or("output.bin")));
        std::fs::write(&local, &bytes)
            .map_err(|err| anyhow!("cannot write {}: {err}", local.display()))?;
        println!(
            "{}",
            cli::render(&json!({
                "transport": sysfs::TRANSPORT,
                "staged_at": remote,
                "local_path": local.display().to_string(),
                "bytes": bytes.len(),
                "command_output": result.output,
            }))
        );
        return Ok(());
    }

    println!("{}", cli::render(&result.to_json()));
    Ok(())
}

async fn cmd_fs(client: &Arc<MuseClient>, args: &Args) -> Result<()> {
    let action = args.positional(1, "action")?;
    match action.as_str() {
        "ls" | "list" => {
            let path = args.positional_opt(2).unwrap_or_default();
            println!("{}", cli::render(&client.fs_list(&path).await?));
        }
        "stat" => {
            println!("{}", cli::render(&client.fs_stat(&args.positional(2, "path")?).await?));
        }
        "cat" | "read" => {
            let path = args.positional(2, "path")?;
            let offset = args.flag_u64("offset")?.unwrap_or(0);
            let len = args.flag_u64("len")?.unwrap_or(262_144) as usize;
            let (bytes, text) = client.fs_read_window(&path, offset, len).await?;
            if args.has("base64") || text.is_none() {
                // The server only decodes to `text` when the window lands on a
                // UTF-8 boundary; a mid-character offset comes back as raw
                // bytes. Say so instead of silently returning base64.
                let reason = if args.has("base64") {
                    "requested"
                } else {
                    "window is not on a UTF-8 character boundary; shift --offset"
                };
                println!(
                    "{}",
                    cli::render(&json!({
                        "path": path,
                        "offset": offset,
                        "bytes": bytes.len(),
                        "encoding": "base64",
                        "why_base64": reason,
                        "content": client::base64_encode(&bytes),
                    }))
                );
            } else {
                println!(
                    "{}",
                    cli::render(&json!({
                        "path": path,
                        "offset": offset,
                        "bytes": bytes.len(),
                        "encoding": "utf8",
                        "content": text.unwrap_or_default(),
                    }))
                );
            }
        }
        "get" | "download" => {
            let remote = args.positional(2, "remote path")?;
            let local = PathBuf::from(args.positional(3, "local path")?);
            let bytes = client.fs_download(&remote).await?;
            if let Some(parent) = local.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            std::fs::write(&local, &bytes)
                .map_err(|err| anyhow!("cannot write {}: {err}", local.display()))?;
            println!(
                "{}",
                cli::render(&json!({
                    "remote": remote,
                    "local": local.display().to_string(),
                    "bytes": bytes.len(),
                }))
            );
        }
        "put" | "upload" => {
            let local = PathBuf::from(args.positional(2, "local path")?);
            let remote = args.positional(3, "remote path")?;
            println!("{}", cli::render(&client.fs_upload(&local, &remote).await?));
        }
        "mkdir" => {
            let path = args.positional(2, "path")?;
            println!(
                "{}",
                cli::render(&client.fs_mkdir(&path, args.has("recursive")).await?)
            );
        }
        "mv" | "rename" => {
            let from = args.positional(2, "from")?;
            let to = args.positional(3, "to")?;
            println!("{}", cli::render(&client.fs_rename(&from, &to).await?));
        }
        "rm" | "delete" => {
            if !args.has("confirm") {
                bail!("deleting workspace files is irreversible; add --confirm");
            }
            let path = args.positional(2, "path")?;
            println!(
                "{}",
                cli::render(&client.fs_delete(&path, args.has("recursive")).await?)
            );
        }
        "search" | "find" => {
            let query = args.positional_opt(2).unwrap_or_default();
            let extensions: Vec<String> = args
                .flag("ext")
                .map(|raw| {
                    raw.split(',')
                        .map(|part| part.trim().trim_start_matches('.').to_string())
                        .filter(|part| !part.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            println!(
                "{}",
                cli::render(&client.fs_search(&query, &extensions).await?)
            );
        }
        "library" => {
            let preset = args.flag("preset").unwrap_or("recents");
            println!("{}", cli::render(&client.fs_library(preset).await?));
        }
        other => bail!(
            "unknown fs action `{other}`; \
             expected ls|stat|cat|get|put|mkdir|mv|rm|search|library"
        ),
    }
    Ok(())
}

async fn cmd_goals(client: &Arc<MuseClient>, args: &Args) -> Result<()> {
    let action = args.positional(1, "action")?;
    match action.as_str() {
        "list" => {
            let mut path = "/api/goals".to_string();
            if args.has("all") {
                path.push_str("?include_archived=true");
            }
            println!("{}", cli::render(&client.raw_rpc("GET", &path, None).await?));
        }
        "get" => {
            let id = args.positional(2, "goal_id")?;
            println!(
                "{}",
                cli::render(&client.raw_rpc("GET", &format!("/api/goals/{id}"), None).await?)
            );
        }
        "create" => {
            let body = json!({
                "title": args.flag("title").ok_or_else(|| anyhow!("--title is required"))?,
                "summary": args.flag("summary").ok_or_else(|| anyhow!("--summary is required"))?,
                "description": args.flag("description").ok_or_else(|| anyhow!("--description is required"))?,
                "emoji": args.flag("emoji").unwrap_or("🎯"),
            });
            println!(
                "{}",
                cli::render(&client.raw_rpc("POST", "/api/goals", Some(body)).await?)
            );
        }
        "update" => {
            let id = args.positional(2, "goal_id")?;
            let mut body = serde_json::Map::new();
            for key in ["title", "summary", "description", "status", "emoji", "momentum"] {
                if let Some(value) = args.flag(key) {
                    body.insert(key.to_string(), json!(value));
                }
            }
            if body.is_empty() {
                bail!("give at least one of --title --summary --description --status --emoji --momentum");
            }
            println!(
                "{}",
                cli::render(
                    &client
                        .raw_rpc("PATCH", &format!("/api/goals/{id}"), Some(Value::Object(body)))
                        .await?
                )
            );
        }
        "delete" => {
            if !args.has("confirm") {
                bail!("deleting a goal is irreversible; add --confirm");
            }
            let id = args.positional(2, "goal_id")?;
            println!(
                "{}",
                cli::render(&client.raw_rpc("DELETE", &format!("/api/goals/{id}"), None).await?)
            );
        }
        other => bail!("unknown goals action `{other}`; expected list|get|create|update|delete"),
    }
    Ok(())
}

async fn cmd_settings(client: &Arc<MuseClient>, args: &Args) -> Result<()> {
    let action = args.positional_opt(1).unwrap_or_else(|| "read".to_string());
    match action.as_str() {
        "read" => {
            let mut out = serde_json::Map::new();
            for (key, path) in [
                ("model", "/model"),
                ("identity", "/api/identity"),
                ("tab_order", "/api/navigation/tab-order"),
                ("product_improvements", "/api/product-improvements"),
                ("voice_active", "/api/voice/active-voice"),
                ("chat_themes", "/api/chat/themes"),
                ("onboarding_tasks", "/api/onboarding/tasks"),
                ("onboarding_variants", "/api/onboarding/variants"),
            ] {
                match client.raw_rpc("GET", path, None).await {
                    Ok(value) => {
                        out.insert(key.to_string(), value);
                    }
                    Err(err) => {
                        out.insert(key.to_string(), json!({ "error": err.to_string() }));
                    }
                }
            }
            println!("{}", cli::render(&Value::Object(out)));
        }
        "set-voice" => {
            let voice = args.positional(2, "voice-id")?;
            println!(
                "{}",
                cli::render(
                    &client
                        .raw_rpc(
                            "POST",
                            "/api/voice/active-voice",
                            Some(json!({ "voice_id": voice })),
                        )
                        .await?
                )
            );
        }
        "set-tab-order" => {
            let order: Vec<String> = args.positionals.iter().skip(2).cloned().collect();
            if order.is_empty() {
                bail!("muse settings set-tab-order <tab>...");
            }
            println!(
                "{}",
                cli::render(
                    &client
                        .raw_rpc(
                            "POST",
                            "/api/navigation/tab-order",
                            Some(json!({ "tab_order": order })),
                        )
                        .await?
                )
            );
        }
        other => bail!("unknown settings action `{other}`; expected read|set-voice|set-tab-order"),
    }
    Ok(())
}

/// Read JSON from `--json`, `--json-file`, or `--json -`.
/// Build the request body from `--set key=value`, `--json`, or `--json-file`.
///
/// `--set` exists because Windows PowerShell 5.1 strips double quotes even
/// inside single quotes before handing the string to a native process:
///
/// ```text
/// '{"method":"/x"}'  ->  {method:/x}      (quotes gone, JSON invalid)
/// ```
///
/// `--set` carries no quoting characters at all, so it survives every shell.
fn json_argument(args: &Args) -> Result<Option<Value>> {
    let sets = args.flag_all("set");
    if !sets.is_empty() {
        let mut map = serde_json::Map::new();
        for entry in sets {
            let (key, raw) = entry
                .split_once('=')
                .ok_or_else(|| anyhow!("--set expects key=value, got `{entry}`"))?;
            let key = key.trim();
            if key.is_empty() {
                bail!("--set needs a key before `=`: `{entry}`");
            }
            map.insert(key.to_string(), parse_scalar(raw.trim()));
        }
        if args.flag("json").is_some() || args.flag("json-file").is_some() {
            bail!("use either --set or --json/--json-file, not both");
        }
        return Ok(Some(Value::Object(map)));
    }

    if let Some(inline) = args.flag("json") {
        if inline == "-" {
            use std::io::Read;
            let mut buffer = String::new();
            std::io::stdin().read_to_string(&mut buffer)?;
            if buffer.trim().is_empty() {
                return Ok(None);
            }
            return Ok(Some(
                serde_json::from_str(&buffer).context("stdin is not valid JSON")?,
            ));
        }
        return Ok(Some(serde_json::from_str(inline).context(
            "--json is not valid JSON (Windows PowerShell strips quotes; use --set key=value \
             or --json-file instead)",
        )?));
    }
    if let Some(path) = args.flag("json-file") {
        let raw = std::fs::read_to_string(path)
            .map_err(|err| anyhow!("cannot read {path}: {err}"))?;
        return Ok(Some(
            serde_json::from_str(&raw).context("file does not contain valid JSON")?,
        ));
    }
    Ok(None)
}

/// Interpret a `--set` value: bare `true`/`false`/`null`, integers, and
/// comma-separated lists become typed JSON; everything else stays a string.
fn parse_scalar(raw: &str) -> Value {
    match raw {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        "null" => return Value::Null,
        _ => {}
    }
    if let Ok(number) = raw.parse::<i64>() {
        return json!(number);
    }
    // `a,b,c` becomes an array so filters and id lists work without quoting.
    if raw.contains(',') {
        return Value::Array(raw.split(',').map(|part| json!(part.trim())).collect());
    }
    json!(raw)
}

async fn cmd_rpc(client: &Arc<MuseClient>, args: &Args) -> Result<()> {
    let method = args.positional(1, "METHOD")?;
    let path = args.positional(2, "path")?;
    let params = json_argument(args)?;
    let destructive = matches!(
        method.to_ascii_uppercase().as_str(),
        "DELETE" | "PUT"
    ) || path.contains("/delete")
        || path.contains("/remove");
    if destructive && !args.has("confirm") {
        bail!("{method} {path} can destroy data; add --confirm");
    }
    println!(
        "{}",
        cli::render(&client.raw_rpc(&method, &path, params).await?)
    );
    Ok(())
}

/// Fetch any gateway path that answers with bytes and write it to a file.
///
/// Most routes answer JSON; this exists for `/fs/export` (a zip of the
/// requested paths), media handles, and anything else whose body is not JSON.
async fn cmd_raw(client: &Arc<MuseClient>, args: &Args) -> Result<()> {
    let path = args.positional(1, "path")?;
    let method = args.flag("method").unwrap_or("GET").to_ascii_uppercase();
    let params = json_argument(args)?;
    let out = args
        .flag("out")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("raw.bin"));

    let timeout = Duration::from_secs(args.flag_u64("timeout")?.unwrap_or(600).max(10));
    let bytes = client.raw_bytes(&method, &path, params, timeout).await?;
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(&out, &bytes)
        .map_err(|err| anyhow!("cannot write {}: {err}", out.display()))?;

    // Report the content type we can infer rather than pretending to know what
    // the server meant it to be.
    let kind = if bytes.starts_with(b"PK\x03\x04") {
        "zip"
    } else if bytes.starts_with(b"%PDF") {
        "pdf"
    } else if bytes.starts_with(b"\x89PNG") {
        "png"
    } else if bytes.starts_with(b"\xff\xd8") {
        "jpeg"
    } else if bytes.iter().take(64).all(|b| b.is_ascii_graphic() || *b == b'\n' || *b == b'\r') {
        "text"
    } else {
        "unknown"
    };

    println!(
        "{}",
        cli::render(&json!({
            "method": method,
            "path": path,
            "out": out.display().to_string(),
            "bytes": bytes.len(),
            "detected": kind,
        }))
    );
    Ok(())
}

/// List or decide the agent approvals that gate tool execution.
async fn cmd_approvals(client: &Arc<MuseClient>, args: &Args) -> Result<()> {
    let action = args.positional_opt(1).unwrap_or_else(|| "list".to_string());
    match action.as_str() {
        "list" => {
            let snapshot = client.approvals().await?;
            let pending = snapshot
                .get("pending")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let recent = snapshot
                .get("recent")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if args.has("json") {
                println!("{}", cli::render(&snapshot));
                return Ok(());
            }
            if pending.is_empty() && recent.is_empty() {
                println!("no approvals");
                return Ok(());
            }
            for entry in &pending {
                println!("{}", approval_line(entry, "pending"));
            }
            for entry in recent.iter().take(20) {
                println!("{}", approval_line(entry, "recent"));
            }
            Ok(())
        }
        "show" => {
            let approval_id = args
                .positional_opt(2)
                .ok_or_else(|| anyhow!("muse approvals show needs an approval id"))?;
            let snapshot = client.approvals().await?;
            let found = find_approval(&snapshot, &approval_id)
                .ok_or_else(|| anyhow!("no approval with id {approval_id}"))?;
            println!("{}", cli::render(&found));
            Ok(())
        }
        "decide" => {
            let approval_id = args
                .positional_opt(2)
                .ok_or_else(|| anyhow!("muse approvals decide needs an approval id"))?;
            let decision = args
                .positional_opt(3)
                .ok_or_else(|| anyhow!("muse approvals decide needs a decision"))?;
            validate_decision(&decision)?;
            if !args.has("confirm") && !is_passive_decision(&decision) {
                bail!(
                    "{decision} grants access beyond this one call; pass --confirm. \
                     Use allow_once for a single request."
                );
            }
            let result = client
                .decide_approvals(json!([{ "approval_id": approval_id, "decision": decision }]))
                .await?;
            println!("{}", cli::render(&decide_summary(&result)));
            Ok(())
        }
        other => bail!("unknown approvals action {other}; expected list/show/decide"),
    }
}

/// Decisions that only affect the request already waiting.
pub(crate) fn is_passive_decision(decision: &str) -> bool {
    matches!(decision, "allow_once" | "deny")
}

pub(crate) const DECISIONS: [&str; 7] = [
    "allow_once",
    "allow_session",
    "allow_today",
    "allow_always",
    "auto_allow",
    "deny",
    "deny_always",
];

fn validate_decision(decision: &str) -> Result<()> {
    if DECISIONS.contains(&decision) {
        return Ok(());
    }
    bail!(
        "unknown decision `{decision}`; expected one of {}",
        DECISIONS.join(", ")
    )
}

/// One line per approval: id, status, host or path, and why it was raised.
fn approval_line(entry: &Value, tag: &str) -> String {
    let id = entry
        .get("approval_id")
        .and_then(Value::as_str)
        .unwrap_or("-");
    let status = entry
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("-");
    let reason = entry
        .get("decision_reason")
        .and_then(Value::as_str)
        .or_else(|| entry.get("reason").and_then(Value::as_str))
        .unwrap_or("");
    let target = entry
        .get("host")
        .and_then(Value::as_str)
        .map(|host| {
            let method = entry.get("method").and_then(Value::as_str).unwrap_or("");
            let path = entry.get("path").and_then(Value::as_str).unwrap_or("");
            format!("{method} {host}{path}")
        })
        .or_else(|| {
            entry
                .get("binary_path")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    format!("{tag:<7} {id} {status:<10} {target} {reason}")
}

fn find_approval(snapshot: &Value, approval_id: &str) -> Option<Value> {
    for key in ["pending", "pending_approvals", "recent"] {
        let Some(list) = snapshot.get(key).and_then(Value::as_array) else {
            continue;
        };
        if let Some(found) = list
            .iter()
            .find(|entry| entry.get("approval_id").and_then(Value::as_str) == Some(approval_id))
        {
            return Some(found.clone());
        }
    }
    None
}

pub(crate) fn decide_summary(result: &Value) -> Value {
    let results = result
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let rows: Vec<Value> = results
        .iter()
        .map(|row| {
            json!({
                "approval_id": row.get("approval_id").cloned().unwrap_or(Value::Null),
                "success": row.get("success").cloned().unwrap_or(Value::Null),
                "error": row.get("error").cloned().unwrap_or(Value::Null),
                "decision": row
                    .get("approval")
                    .and_then(|approval| approval.get("decision"))
                    .cloned()
                    .unwrap_or(Value::Null),
                "decision_reason": row
                    .get("approval")
                    .and_then(|approval| approval.get("decision_reason"))
                    .cloned()
                    .unwrap_or(Value::Null),
            })
        })
        .collect();
    json!({ "decided": rows.len(), "results": rows })
}

/// Call a route on a specific gateway service (`muse rpc` always uses the
/// daemon).
async fn cmd_service(client: &Arc<MuseClient>, args: &Args) -> Result<()> {
    let service = args.positional(1, "SERVICE")?;
    let service: u64 = service
        .parse()
        .map_err(|_| anyhow!("SERVICE must be a number; got {service}"))?;
    let method = args.positional(2, "METHOD")?.to_ascii_uppercase();
    let path = args.positional(3, "path")?;
    let params = json_argument(args)?;
    let value = client.raw_rpc_on(&method, &path, params, service).await?;
    println!("{}", cli::render(&value));
    Ok(())
}

async fn cmd_web(client: &Arc<MuseClient>, args: &Args) -> Result<()> {
    let method = args.positional(1, "METHOD")?;
    let path = args.positional(2, "path")?;
    let body = json_argument(args)?;
    let mut http = client.http_mut().await;
    println!(
        "{}",
        tools::render_public(&http.web_api(&method, &path, body, args.has("csrf")).await?)
    );
    Ok(())
}

async fn cmd_call(client: &Arc<MuseClient>, args: &Args) -> Result<()> {
    let tool = args.positional(1, "tool")?;
    let arguments = json_argument(args)?.unwrap_or_else(|| json!({}));
    println!("{}", tools::call(client, &tool, arguments).await?);
    Ok(())
}
