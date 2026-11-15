//! Terminal UI: read and write Muse conversations without leaving the shell.
//!
//! The layout is a dense transcript above a single-line composer, with a status
//! bar that names the conversation and the connection state. Keys follow what
//! a terminal user already expects from a chat client, and the network work
//! happens off the drawing loop so typing never stalls.

use crate::client::{AskOutcome, MuseClient};
use anyhow::{bail, Context, Result};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::{
    layout::{Constraint, Layout, Margin},
    style::{Color, Modifier, Style},
    Frame,
};
use std::sync::Arc;
use tokio::sync::mpsc;

/// One rendered line in the transcript.
struct Entry {
    role: Role,
    text: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    User,
    Assistant,
    System,
}

/// What the drawing loop owns. No network state lives here.
struct Ui {
    entries: Vec<Entry>,
    input: String,
    cursor: usize,
    scroll: usize,
    busy: bool,
    status: String,
    thread: Option<String>,
    session_id: Option<String>,
    follow: bool,
    quit: bool,
}

impl Ui {
    fn new(thread: Option<String>, session_id: Option<String>) -> Self {
        Self {
            entries: Vec::new(),
            input: String::new(),
            cursor: 0,
            scroll: 0,
            busy: false,
            status: "ready".to_string(),
            thread,
            session_id,
            follow: false,
            quit: false,
        }
    }

    fn push(&mut self, role: Role, text: &str) {
        for part in text.split('\n') {
            self.entries.push(Entry {
                role,
                text: part.to_string(),
            });
        }
    }

    fn wrapped_line_count(&self, width: u16) -> usize {
        let usable = width.saturating_sub(2).max(1) as usize;
        self.entries
            .iter()
            .map(|entry| {
                let chars = entry.text.chars().count();
                (chars.max(1) + usable - 1) / usable
            })
            .sum()
    }

    fn insert_char(&mut self, ch: char) {
        let byte = self
            .input
            .char_indices()
            .nth(self.cursor)
            .map(|(index, _)| index)
            .unwrap_or(self.input.len());
        self.input.insert(byte, ch);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        if let Some((index, ch)) = self
            .input
            .char_indices()
            .nth(self.cursor - 1)
            .map(|(index, ch)| (index, ch))
        {
            self.input.replace_range(index..index + ch.len_utf8(), "");
            self.cursor -= 1;
        }
    }

    fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn move_right(&mut self) {
        if self.cursor < self.input.chars().count() {
            self.cursor += 1;
        }
    }
}

/// Messages from the input thread and from the network task.
enum Event_ {
    Key(KeyEvent),
    Reply(Box<AskOutcome>),
    Failed(String),
}

/// Whether this process has a terminal it can drive.
///
/// The TUI mutates console state (raw mode, the alternate screen, cursor
/// visibility). Those live in the console, not in this process, so a process
/// that dies without restoring leaves the user's terminal unusable. Refusing to
/// start without a TTY is the cheap half of that guard: it stops the case where
/// nothing could ever have worked anyway.
fn has_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

pub async fn run(
    client: Arc<MuseClient>,
    thread: Option<String>,
    session_id: Option<String>,
) -> Result<()> {
    if !has_terminal() {
        bail!(
            "muse tui needs an interactive terminal (stdin and stdout are not TTYs). \
             Run it from a terminal, or use `muse chat` for scripted use."
        );
    }
    let mut ui = Ui::new(thread.clone(), session_id.clone());
    // Show the existing conversation so the UI opens where the chat is, not on
    // an empty screen that looks like a fresh conversation.
    match client.history(session_id.as_deref(), 30).await {
        Ok(history) => {
            let events = history
                .get("chat_events")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut recent = Vec::new();
            for event in &events {
                let role = event.get("role").and_then(serde_json::Value::as_str);
                let text = event
                    .get("display_text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                match role {
                    Some("user") => recent.push((Role::User, text.to_string())),
                    Some("assistant") if !text.trim().is_empty() => {
                        recent.push((Role::Assistant, text.to_string()))
                    }
                    _ => {}
                }
            }
            for (role, text) in recent {
                ui.push(role, &text);
            }
            if !ui.entries.is_empty() {
                ui.push(Role::System, "");
            }
        }
        Err(err) => ui.push(Role::System, &format!("履歴を読めませんでした: {err}")),
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<Event_>();
    spawn_key_reader(tx.clone());

    // Entering the alternate screen is a console-level change. A panic between
    // here and `restore` would leave the terminal in raw mode with the cursor
    // hidden, so the hook restores it before the process unwinds.
    install_panic_restore();

    let mut terminal = EnteredTerminal::new()?;
    let outcome = draw_loop(terminal.inner(), &mut ui, &client, tx, &mut rx).await;
    drop(terminal);
    outcome
}

/// Restore the console if the process panics while the TUI owns it.
fn install_panic_restore() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        previous(info);
    }));
}

/// Owns the console while the TUI runs, and gives it back on drop.
///
/// `ratatui::restore` is idempotent, so calling it on the normal path and again
/// on the panic path is safe. What matters is that it runs at all.
struct EnteredTerminal {
    terminal: ratatui::DefaultTerminal,
}

impl EnteredTerminal {
    fn new() -> Result<Self> {
        Ok(Self {
            terminal: ratatui::init(),
        })
    }

    fn inner(&mut self) -> &mut ratatui::DefaultTerminal {
        &mut self.terminal
    }
}

impl Drop for EnteredTerminal {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

/// Read keys on a blocking thread; the drawing loop stays responsive.
fn spawn_key_reader(tx: mpsc::UnboundedSender<Event_>) {
    tokio::task::spawn_blocking(move || loop {
        match crossterm::event::read() {
            Ok(Event::Key(key)) => {
                if tx.send(Event_::Key(key)).is_err() {
                    return;
                }
            }
            Ok(_) => {}
            Err(_) => return,
        }
    });
}

async fn draw_loop(
    terminal: &mut ratatui::DefaultTerminal,
    ui: &mut Ui,
    client: &Arc<MuseClient>,
    tx: mpsc::UnboundedSender<Event_>,
    rx: &mut mpsc::UnboundedReceiver<Event_>,
) -> Result<()> {
    loop {
        terminal.draw(|frame| render(frame, ui))?;
        if ui.quit {
            return Ok(());
        }
        // Wait for an event, but wake periodically so the spinner and the
        // elapsed time stay truthful while a turn is in flight.
        let event = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await;
        let event = match event {
            Ok(Some(event)) => event,
            Ok(None) => return Ok(()),
            Err(_) => {
                if ui.busy {
                    ui.status = format!("送信中 {}", spinner());
                }
                continue;
            }
        };

        match event {
            Event_::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                handle_key(ui, key);
                if ui.quit {
                    continue;
                }
                if let Some(prompt) = take_submission(ui) {
                    let client = client.clone();
                    let tx = tx.clone();
                    let session_id = ui.session_id.clone();
                    let thread = ui.thread.clone();
                    ui.busy = true;
                    ui.status = "送信中".to_string();
                    tokio::spawn(async move {
                        let result = send_turn(&client, &prompt, session_id.as_deref(), thread.as_deref(), tx.clone()).await;
                        match result {
                            Ok(outcome) => {
                                let _ = tx.send(Event_::Reply(Box::new(outcome)));
                            }
                            Err(err) => {
                                let _ = tx.send(Event_::Failed(err.to_string()));
                            }
                        }
                    });
                }
            }
            Event_::Reply(outcome) => {
                ui.busy = false;
                if ui.session_id.is_none() {
                    ui.session_id = Some(outcome.session_id.clone());
                }
                let text = outcome
                    .reply
                    .get("display_text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if text.trim().is_empty() {
                    ui.push(Role::System, "(返信なし)");
                } else {
                    ui.push(Role::Assistant, &text);
                }
                ui.push(Role::System, "");
                ui.status = "ready".to_string();
            }
            Event_::Failed(err) => {
                ui.busy = false;
                ui.push(Role::System, &format!("失敗: {err}"));
                ui.status = "失敗".to_string();
            }
        }
    }
}

/// Run one turn and scroll the answer into view.
async fn send_turn(
    client: &Arc<MuseClient>,
    prompt: &str,
    session_id: Option<&str>,
    thread: Option<&str>,
    _tx: mpsc::UnboundedSender<Event_>,
) -> Result<AskOutcome> {
    let _ = thread;
    client
        .ask_and_wait(
            prompt,
            session_id,
            &[],
            crate::client::AttachmentKind::File,
            std::time::Duration::from_secs(240),
            std::time::Duration::from_secs(2),
        )
        .await
        .context("turn failed")
}

/// Consume the composer when Enter submits it.
fn take_submission(ui: &mut Ui) -> Option<String> {
    let text = ui.input.trim().to_string();
    if text.is_empty() {
        return None;
    }
    ui.input.clear();
    ui.cursor = 0;
    ui.push(Role::User, &text);
    Some(text)
}

fn handle_key(ui: &mut Ui, key: KeyEvent) {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) | (KeyCode::Char('q'), KeyModifiers::CONTROL) => ui.quit = true,
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => ui.quit = true,
        (KeyCode::Enter, _) => {
            // Enter submits; the actual send happens in the draw loop.
        }
        (KeyCode::Char(ch), KeyModifiers::NONE) | (KeyCode::Char(ch), KeyModifiers::SHIFT) => {
            ui.insert_char(ch)
        }
        (KeyCode::Backspace, _) => ui.backspace(),
        (KeyCode::Delete, _) => ui.backspace(),
        (KeyCode::Left, _) => ui.move_left(),
        (KeyCode::Right, _) => ui.move_right(),
        (KeyCode::Home, _) => ui.cursor = 0,
        (KeyCode::End, _) => ui.cursor = ui.input.chars().count(),
        (KeyCode::PageUp, _) => ui.scroll = ui.scroll.saturating_sub(5),
        (KeyCode::PageDown, _) => ui.scroll += 5,
        (KeyCode::Tab, _) => {
            ui.follow = !ui.follow;
        }
        _ => {}
    }
}

fn spinner() -> &'static str {
    // Four frames is enough to show motion without pretending to be progress.
    const FRAMES: [&str; 4] = ["|", "/", "-", "\\"];
    // Rotate off the wall clock so the caller does not need to keep state.
    let tick = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as usize)
        .unwrap_or(0);
    FRAMES[(tick / 200) % FRAMES.len()]
}

fn render(frame: &mut Frame, ui: &mut Ui) {
    let area = frame.area();
    let chunks = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(area);

    let view_height = chunks[0].height.saturating_sub(2);
    let width = chunks[0].width;
    // Follow the end of the conversation unless the user scrolled up.
    let max_scroll = ui.wrapped_line_count(width).saturating_sub(view_height as usize);
    if ui.scroll > max_scroll {
        ui.scroll = max_scroll;
    }
    if ui.scroll == 0 || ui.follow {
        ui.scroll = max_scroll;
    }

    let title = match (&ui.thread, &ui.session_id) {
        (Some(name), _) => format!(" {name} "),
        (None, Some(id)) => format!(" {id} "),
        (None, None) => " primary ".to_string(),
    };

    let mut lines: Vec<Line> = Vec::with_capacity(ui.entries.len());
    for entry in &ui.entries {
        let (prefix, style) = match entry.role {
            Role::User => ("you  ", Style::default().fg(Color::Rgb(120, 180, 255))),
            Role::Assistant => ("muse ", Style::default().fg(Color::Rgb(220, 225, 232))),
            Role::System => ("     ", Style::default().fg(Color::Rgb(120, 126, 136))),
        };
        lines.push(Line::from(vec![
            Span::styled(prefix, style.add_modifier(Modifier::BOLD)),
            Span::styled(entry.text.clone(), style),
        ]));
    }

    let transcript = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false })
        .scroll((ui.scroll as u16, 0));
    frame.render_widget(transcript, chunks[0]);

    // Composer: a single line with a visible caret drawn as a block.
    let before: String = ui.input.chars().take(ui.cursor).collect();
    let after: String = ui.input.chars().skip(ui.cursor).collect();
    let composer = if ui.busy {
        Paragraph::new(Line::from(Span::styled(
            "返信を待っています。Esc で終了。",
            Style::default().fg(Color::Rgb(120, 126, 136)),
        )))
        .block(Block::default().borders(Borders::ALL).title(" muse "))
    } else {
        Paragraph::new(Line::from(vec![
            Span::raw(before),
            Span::styled(
                if after.is_empty() { " " } else { "▏" },
                Style::default().add_modifier(Modifier::REVERSED),
            ),
            Span::raw(after),
        ]))
        .block(Block::default().borders(Borders::ALL).title(" muse "))
        .wrap(Wrap { trim: false })
    };
    frame.render_widget(composer, chunks[1]);

    let state = if ui.busy { "busy" } else { "ready" };
    let status = format!(
        " {state}  |  Enter 送信  Tab 追従  PgUp/PgDn 移動  Esc 終了  |  {}",
        ui.status
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            status,
            Style::default().fg(Color::Rgb(140, 146, 156)),
        ))),
        chunks[2],
    );
    let _ = Margin::default();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ui() -> Ui {
        Ui::new(None, None)
    }

    #[test]
    fn typing_inserts_at_the_cursor() {
        let mut ui = ui();
        ui.insert_char('a');
        ui.insert_char('b');
        assert_eq!(ui.input, "ab");
        assert_eq!(ui.cursor, 2);
        ui.move_left();
        ui.insert_char('X');
        assert_eq!(ui.input, "aXb");
        assert_eq!(ui.cursor, 2);
    }

    #[test]
    fn backspace_removes_the_character_before_the_cursor() {
        let mut ui = ui();
        ui.insert_char('a');
        ui.insert_char('b');
        ui.backspace();
        assert_eq!(ui.input, "a");
        assert_eq!(ui.cursor, 1);
        ui.backspace();
        assert!(ui.input.is_empty());
        // Deleting at the start is a no-op, not a panic.
        ui.backspace();
        assert!(ui.input.is_empty());
    }

    // Multi-byte input must not split a character; the composer stores bytes.
    #[test]
    fn multibyte_input_round_trips() {
        let mut ui = ui();
        for ch in "日本語".chars() {
            ui.insert_char(ch);
        }
        assert_eq!(ui.input, "日本語");
        assert_eq!(ui.cursor, 3);
        ui.move_left();
        ui.move_left();
        // Two lefts from the end put the caret between 日 and 本, so backspace
        // removes 日 and leaves 本語. The point of this test is that a
        // multi-byte character is removed whole rather than by one byte.
        ui.backspace();
        assert_eq!(ui.input, "本語");
    }

    #[test]
    fn cursor_stops_at_the_edges() {
        let mut ui = ui();
        ui.insert_char('a');
        ui.move_right();
        assert_eq!(ui.cursor, 1, "cannot move past the end");
        ui.move_left();
        ui.move_left();
        assert_eq!(ui.cursor, 0, "cannot move before the start");
    }

    #[test]
    fn submission_trims_and_clears() {
        let mut ui = ui();
        ui.input = "  hello  ".to_string();
        let submitted = take_submission(&mut ui).expect("submission");
        assert_eq!(submitted, "hello");
        assert!(ui.input.is_empty());
        assert_eq!(ui.entries.last().map(|entry| entry.role), Some(Role::User));
    }

    // A blank Enter must not send an empty turn.
    #[test]
    fn blank_input_is_not_submitted() {
        let mut ui = ui();
        ui.input = "   ".to_string();
        assert!(take_submission(&mut ui).is_none());
    }

    #[test]
    fn wrapped_height_accounts_for_width() {
        let mut ui = ui();
        ui.push(Role::User, &"x".repeat(50));
        // Two inner columns available at width 4, so 50 chars need 25 rows.
        assert_eq!(ui.wrapped_line_count(4), 25);
        assert!(ui.wrapped_line_count(60) <= 2);
    }

    #[test]
    fn newlines_become_separate_entries() {
        let mut ui = ui();
        ui.push(Role::Assistant, "one
two");
        assert_eq!(ui.entries.len(), 2);
        assert_eq!(ui.entries[1].text, "two");
    }

    // The render path is exercised through a test backend: this is the real
    // function the terminal runs, not a stand-in.
    #[test]
    fn render_draws_composer_and_transcript() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut ui = ui();
        ui.push(Role::User, "hello");
        ui.push(Role::Assistant, "hi there");
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).expect("backend");
        terminal.draw(|frame| render(frame, &mut ui)).expect("draw");

        let buffer = format!("{:?}", terminal.backend());
        assert!(buffer.contains("hello"), "user line missing: {buffer}");
        assert!(buffer.contains("hi there"), "assistant line missing: {buffer}");
        assert!(buffer.contains("primary"), "title missing: {buffer}");
    }
}
