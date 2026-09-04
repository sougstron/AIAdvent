//! ChatGPT-style console chat, built on ratatui: a scrollback transcript, an
//! input line, and slash commands for sessions, effort, JSON mode, length
//! and stop-condition settings.

use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use serde_json::Value;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::api::{self, ChatMessage, Endpoint, Outcome};
use crate::config::{self, Effort, Res, Settings};
use crate::render::{self, strip_fences};
use crate::session::{self, Session, SessionSummary};
use crate::verify;

const MAX_CHARS_CHOICES: &[Option<usize>] =
    &[None, Some(100), Some(250), Some(500), Some(1000), Some(2000)];
const BUDGET_CHOICES: &[Option<u32>] =
    &[None, Some(32), Some(64), Some(128), Some(256), Some(512), Some(1024), Some(4096)];
const STOP_PRESETS: &[&[&str]] = &[&[], &["\n\n"], &["\n---\n"], &["\n\n\n"]];
/// Up to the provider's hard ceiling (`config::TEMP_MAX`); 2.01 is rejected
/// with HTTP 400, so 2.0 is the last usable step.
const TEMP_CHOICES: &[Option<f32>] = &[
    None,
    Some(0.0),
    Some(0.3),
    Some(0.7),
    Some(1.0),
    Some(1.2),
    Some(1.5),
    Some(2.0),
];
const TOP_P_CHOICES: &[Option<f32>] = &[None, Some(0.5), Some(0.8), Some(0.95), Some(1.0)];
const TOP_K_CHOICES: &[Option<i32>] = &[None, Some(20), Some(50), Some(100), Some(-1)];
const SETTINGS_ROWS: &[&str] = &[
    "effort",
    "json mode",
    "max_chars",
    "budget_tokens",
    "stop",
    "temperature",
    "top_p",
    "top_k",
];
/// Slash commands offered by the input popup, kept in alphabetical order
/// since that's the order the popup lists them in.
const COMMANDS: &[&str] = &[
    "effort", "help", "json", "new", "personas", "quit", "sessions", "settings", "stop", "temp",
    "verify",
];
/// Cycled while a background call is in flight — drawn inline in the
/// transcript instead of a full-screen "working" overlay.
const SPINNER_FRAMES: &[&str] = &["/", "-", "\\", "-"];
/// Maximum visible content rows of the input box; the box grows from 1 to
/// this many rows as the wrapped text gets longer, then scrolls internally.
const MAX_INPUT_LINES: usize = 4;
/// Default cast for `/personas` when the user doesn't supply their own list.
const DEFAULT_PERSONAS: &[&str] = &["physicist", "philosopher", "mathematician"];

/// One event from the background thread reading a streamed reply — lets the
/// main loop redraw an animated spinner on every poll timeout instead of
/// blocking silently on the socket read (which is what used to freeze the
/// screen on a static "model is thinking" overlay during long reasoning
/// pauses between visible tokens).
enum StreamEvent {
    Piece(String),
    Done(Res<Outcome>),
}

fn default_personas() -> Vec<String> {
    DEFAULT_PERSONAS.iter().map(|s| s.to_string()).collect()
}

pub fn run(settings: Settings) -> Res<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("interactive chat requires a terminal".into());
    }
    let ep = Endpoint::resolve()?;
    let mut terminal = ratatui::try_init().map_err(|e| {
        let _ = ratatui::try_restore();
        format!("cannot start TUI: {e}")
    })?;
    // Mouse capture for wheel scrolling; kitty keyboard flags so Shift+Enter
    // arrives as Enter+SHIFT instead of a plain Enter (terminals without
    // support ignore the push and Shift+Enter degrades to Enter); bracketed
    // paste so pasted multi-line text arrives as one Paste event instead of
    // a burst of Char/Enter keys that would send each line separately.
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        EnableMouseCapture,
        EnableBracketedPaste,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    let result = App::new(ep, settings).event_loop(&mut terminal);
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        PopKeyboardEnhancementFlags,
        DisableMouseCapture,
        DisableBracketedPaste
    );
    ratatui::restore();
    result
}

enum Entry {
    User(String),
    Assistant { text: String, note: Option<String> },
    Info(String),
}

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Input,
    Settings,
    Sessions,
}

struct App {
    ep: Endpoint,
    settings: Settings,
    session: Session,
    sessions_dir: PathBuf,
    entries: Vec<Entry>,
    input: String,
    /// Cursor position inside `input`, in chars (not bytes).
    cursor: usize,
    /// First visual (wrapped) input line shown when the text needs more than
    /// the 4 visible rows of the input box.
    input_scroll: usize,
    focus: Focus,
    status: String,
    scroll: u16,
    /// While true, newly appended transcript content keeps the view pinned to
    /// the bottom; any manual scroll up clears it until the user scrolls
    /// back down to the last line.
    follow: bool,
    settings_selected: usize,
    sessions_list: Vec<SessionSummary>,
    sessions_selected: usize,
    /// Index into `sessions_list` awaiting a y/n confirmation before delete.
    sessions_pending_delete: Option<usize>,
    cmd_popup_dismissed: bool,
    cmd_selected: usize,
    quit: bool,
    /// `Some((label, frame))` while a background request is in flight —
    /// rendered as the last line of the transcript, replacing the old
    /// full-screen "working" overlay.
    spinner: Option<(String, &'static str)>,
}

impl App {
    fn new(ep: Endpoint, settings: Settings) -> App {
        App {
            ep,
            session: Session::new(settings.clone()),
            settings,
            sessions_dir: session::sessions_dir(),
            entries: Vec::new(),
            input: String::new(),
            cursor: 0,
            input_scroll: 0,
            focus: Focus::Input,
            status: "Type a message and press Enter · /help for commands".into(),
            scroll: 0,
            follow: true,
            settings_selected: 0,
            sessions_list: Vec::new(),
            sessions_selected: 0,
            sessions_pending_delete: None,
            cmd_popup_dismissed: false,
            cmd_selected: 0,
            quit: false,
            spinner: None,
        }
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> Res<()> {
        while !self.quit {
            terminal
                .draw(|f| self.draw(f))
                .map_err(|e| format!("draw failed: {e}"))?;
            if !event::poll(Duration::from_millis(200)).map_err(|e| e.to_string())? {
                continue;
            }
            match event::read().map_err(|e| e.to_string())? {
                Event::Key(key) => self.handle_key(key, terminal),
                Event::Mouse(mouse) => self.handle_mouse(mouse.kind, terminal),
                Event::Paste(text) => self.handle_paste(&text, terminal),
                _ => continue,
            }
        }
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent, terminal: &mut DefaultTerminal) {
        if key.kind != KeyEventKind::Press {
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('n') => self.new_chat(),
                KeyCode::Char('q') => self.quit = true,
                _ => {}
            }
            return;
        }

        match self.focus {
            Focus::Input => self.handle_input_key(key, terminal),
            Focus::Settings => self.handle_settings_key(key.code),
            Focus::Sessions => self.handle_sessions_key(key.code, terminal),
        }
    }

    /// Mouse wheel scrolls the transcript regardless of which panel has
    /// focus (3 lines per tick).
    fn handle_mouse(&mut self, kind: MouseEventKind, terminal: &DefaultTerminal) {
        match kind {
            MouseEventKind::ScrollUp => {
                let max = self.max_scroll(terminal);
                self.set_scroll(self.scroll.min(max).saturating_sub(3), max);
            }
            MouseEventKind::ScrollDown => {
                let max = self.max_scroll(terminal);
                self.set_scroll(self.scroll.min(max).saturating_add(3), max);
            }
            _ => {}
        }
    }

    /// Sets the transcript scroll offset, clamped to `max`, and refreshes
    /// `follow`: pinned only when the view sits exactly at the last page, so
    /// any manual scroll up detaches from incoming content and scrolling
    /// back to the bottom re-attaches.
    fn set_scroll(&mut self, scroll: u16, max: u16) {
        self.scroll = scroll.min(max);
        self.follow = self.scroll >= max;
    }

    fn handle_input_key(&mut self, key: KeyEvent, terminal: &mut DefaultTerminal) {
        // The command popup only hijacks the arrow keys; Char/Backspace/Enter/
        // Tab/Esc fall through unchanged so typing and the existing bindings
        // keep working while the popup is open.
        if self.command_popup_active() {
            match key.code {
                KeyCode::Up => {
                    self.move_command_selection(-1);
                    return;
                }
                KeyCode::Down => {
                    self.move_command_selection(1);
                    return;
                }
                KeyCode::Left => {
                    self.cmd_popup_dismissed = true;
                    return;
                }
                KeyCode::Right => {
                    self.apply_selected_command(terminal);
                    return;
                }
                _ => {}
            }
        }

        let width = terminal.size().map(|s| s.width).unwrap_or(80);
        match key.code {
            KeyCode::Esc => self.quit = true,
            KeyCode::Tab => {
                self.focus = Focus::Settings;
                self.settings_selected = 0;
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.insert_char('\n');
                self.cmd_selected = 0;
                self.sync_input_scroll(width);
            }
            KeyCode::Enter if !self.input.trim().is_empty() => {
                let line = std::mem::take(&mut self.input);
                self.cursor = 0;
                self.input_scroll = 0;
                self.cmd_popup_dismissed = false;
                self.cmd_selected = 0;
                if let Some(cmd) = line.trim().strip_prefix('/') {
                    self.handle_command(cmd.trim(), terminal);
                } else {
                    self.send_message(line.trim().to_string(), terminal);
                }
            }
            KeyCode::Backspace => {
                self.delete_char_before();
                if self.input.is_empty() {
                    self.cmd_popup_dismissed = false;
                }
                self.cmd_selected = 0;
                self.sync_input_scroll(width);
            }
            KeyCode::Delete => {
                self.delete_char_at();
                self.cmd_selected = 0;
                self.sync_input_scroll(width);
            }
            KeyCode::Char(c) => {
                self.insert_char(c);
                self.cmd_selected = 0;
                self.sync_input_scroll(width);
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                self.sync_input_scroll(width);
            }
            KeyCode::Right => {
                let len = self.input.chars().count();
                if self.cursor < len {
                    self.cursor += 1;
                }
                self.sync_input_scroll(width);
            }
            // Up/Down walk the cursor through the wrapped input lines; once
            // it is already on the first/last line (or the input is a single
            // line), the key scrolls the transcript instead.
            KeyCode::Up => {
                match move_cursor_line(&input_lines(&self.input, width), self.cursor, -1) {
                    Some(pos) => {
                        self.cursor = pos;
                        self.sync_input_scroll(width);
                    }
                    None => {
                        let max = self.max_scroll(terminal);
                        self.set_scroll(self.scroll.min(max).saturating_sub(1), max);
                    }
                }
            }
            KeyCode::Down => {
                match move_cursor_line(&input_lines(&self.input, width), self.cursor, 1) {
                    Some(pos) => {
                        self.cursor = pos;
                        self.sync_input_scroll(width);
                    }
                    None => {
                        let max = self.max_scroll(terminal);
                        self.set_scroll(self.scroll.min(max).saturating_add(1), max);
                    }
                }
            }
            KeyCode::PageUp => {
                let max = self.max_scroll(terminal);
                self.set_scroll(self.scroll.min(max).saturating_sub(10), max);
            }
            KeyCode::PageDown => {
                let max = self.max_scroll(terminal);
                self.set_scroll(self.scroll.min(max).saturating_add(10), max);
            }
            KeyCode::Home => self.set_scroll(0, self.max_scroll(terminal)),
            KeyCode::End => {
                let max = self.max_scroll(terminal);
                self.set_scroll(max, max);
            }
            _ => {}
        }
    }

    /// Bracketed-paste handler: inserts the whole pasted text at the cursor
    /// in one go — newlines included — so a multi-line paste lands in the
    /// input box as a single message instead of being sent line by line.
    fn handle_paste(&mut self, text: &str, terminal: &mut DefaultTerminal) {
        if self.focus != Focus::Input {
            return;
        }
        let width = terminal.size().map(|s| s.width).unwrap_or(80);
        let text = normalize_paste(text);
        let byte = byte_pos(&self.input, self.cursor);
        self.input.insert_str(byte, &text);
        self.cursor += text.chars().count();
        self.cmd_selected = 0;
        self.sync_input_scroll(width);
    }

    /// Inserts `c` at the cursor (which is a char offset; the `String` API
    /// wants a byte offset).
    fn insert_char(&mut self, c: char) {
        let byte = byte_pos(&self.input, self.cursor);
        self.input.insert(byte, c);
        self.cursor += 1;
    }

    fn delete_char_before(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let from = byte_pos(&self.input, self.cursor - 1);
        let to = byte_pos(&self.input, self.cursor);
        self.input.drain(from..to);
        self.cursor -= 1;
    }

    fn delete_char_at(&mut self) {
        if self.cursor >= self.input.chars().count() {
            return;
        }
        let from = byte_pos(&self.input, self.cursor);
        let to = byte_pos(&self.input, self.cursor + 1);
        self.input.drain(from..to);
    }

    /// The input box grows with the wrapped text, from 1 to 4 content rows
    /// (plus its two borders); longer text scrolls inside those 4 rows.
    fn input_box_height(&self, width: u16) -> u16 {
        input_lines(&self.input, width).len().clamp(1, MAX_INPUT_LINES) as u16 + 2
    }

    /// Keeps the cursor's visual row inside the visible input window after
    /// any edit or cursor move.
    fn sync_input_scroll(&mut self, width: u16) {
        let lines = input_lines(&self.input, width);
        let (row, _) = cursor_visual_pos(&lines, self.cursor);
        let view = MAX_INPUT_LINES.min(lines.len().max(1));
        if row < self.input_scroll {
            self.input_scroll = row;
        } else if row >= self.input_scroll + view {
            self.input_scroll = row + 1 - view;
        }
        self.input_scroll = self.input_scroll.min(lines.len().saturating_sub(view));
    }

    /// Whether the input line is currently in "/" command-selection mode:
    /// still composing the command name (no space yet), not dismissed with
    /// Left, and at least one command matches the typed prefix.
    fn command_popup_active(&self) -> bool {
        !self.cmd_popup_dismissed
            && self.input.starts_with('/')
            && !self.input[1..].contains(char::is_whitespace)
            && !self.filtered_commands().is_empty()
    }

    /// Commands matching the text typed after "/", alphabetically ordered
    /// (mirrors `COMMANDS`), case-insensitive prefix match.
    fn filtered_commands(&self) -> Vec<&'static str> {
        let prefix = self.input.strip_prefix('/').unwrap_or("").to_ascii_lowercase();
        COMMANDS.iter().copied().filter(|c| c.starts_with(prefix.as_str())).collect()
    }

    fn move_command_selection(&mut self, delta: i32) {
        let n = self.filtered_commands().len();
        if n == 0 {
            return;
        }
        let i = self.cmd_selected.min(n - 1) as i32;
        self.cmd_selected = (i + delta).rem_euclid(n as i32) as usize;
    }

    fn apply_selected_command(&mut self, terminal: &mut DefaultTerminal) {
        let cmds = self.filtered_commands();
        let Some(name) = cmds.get(self.cmd_selected.min(cmds.len().saturating_sub(1))) else {
            return;
        };
        let name = name.to_string();
        self.input.clear();
        self.cmd_popup_dismissed = false;
        self.cmd_selected = 0;
        self.handle_command(&name, terminal);
    }

    fn handle_settings_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc | KeyCode::Tab => self.focus = Focus::Input,
            KeyCode::Up | KeyCode::Char('k') => {
                self.settings_selected = (self.settings_selected + SETTINGS_ROWS.len() - 1)
                    % SETTINGS_ROWS.len()
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.settings_selected = (self.settings_selected + 1) % SETTINGS_ROWS.len()
            }
            KeyCode::Left | KeyCode::Char('h') => self.adjust_setting(-1),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') | KeyCode::Enter => {
                self.adjust_setting(1)
            }
            _ => {}
        }
    }

    fn handle_sessions_key(&mut self, code: KeyCode, terminal: &mut DefaultTerminal) {
        if self.sessions_pending_delete.is_some() {
            self.handle_sessions_confirm_key(code);
            return;
        }
        match code {
            KeyCode::Esc => self.focus = Focus::Input,
            KeyCode::Up | KeyCode::Char('k') => {
                if !self.sessions_list.is_empty() {
                    self.sessions_selected =
                        (self.sessions_selected + self.sessions_list.len() - 1) % self.sessions_list.len();
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.sessions_list.is_empty() {
                    self.sessions_selected = (self.sessions_selected + 1) % self.sessions_list.len();
                }
            }
            KeyCode::Enter => self.load_selected_session(terminal),
            KeyCode::Char('d') | KeyCode::Delete if !self.sessions_list.is_empty() => {
                self.sessions_pending_delete = Some(self.sessions_selected);
            }
            _ => {}
        }
    }

    /// While a delete confirmation popup is showing: y/Enter deletes,
    /// n/Esc/anything else cancels back to the plain sessions list.
    fn handle_sessions_confirm_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.delete_selected_session(),
            _ => self.sessions_pending_delete = None,
        }
    }

    fn delete_selected_session(&mut self) {
        let Some(idx) = self.sessions_pending_delete.take() else { return };
        let Some(summary) = self.sessions_list.get(idx) else { return };
        let id = summary.id.clone();
        let title = summary.title.clone();
        match session::delete_session(&self.sessions_dir, &id) {
            Ok(()) => {
                if self.session.id == id {
                    // The chat currently open was the one deleted — detach it
                    // from disk so the next autosave doesn't resurrect the file.
                    self.session = Session::new(self.settings.clone());
                    self.entries.clear();
                }
                self.sessions_list.remove(idx);
                if self.sessions_selected >= self.sessions_list.len() {
                    self.sessions_selected = self.sessions_list.len().saturating_sub(1);
                }
                self.status = format!("deleted '{title}'");
            }
            Err(e) => self.status = format!("delete failed: {e}"),
        }
    }

    fn new_chat(&mut self) {
        self.save_session();
        self.session = Session::new(self.settings.clone());
        self.entries.clear();
        self.scroll = 0;
        self.follow = true;
        self.focus = Focus::Input;
        self.status = "New chat".into();
    }

    fn save_session(&self) {
        if self.session.messages.is_empty() {
            return;
        }
        let mut s = self.session.clone();
        s.settings = self.settings.clone();
        if let Err(e) = s.save(&self.sessions_dir) {
            // Best-effort: a save failure should not lose the in-memory chat.
            eprintln!("warning: could not save session: {e}");
        }
    }

    fn load_selected_session(&mut self, terminal: &mut DefaultTerminal) {
        let Some(summary) = self.sessions_list.get(self.sessions_selected) else {
            self.focus = Focus::Input;
            return;
        };
        match session::load_session(&self.sessions_dir, &summary.id) {
            Ok(s) => {
                self.save_session();
                self.settings = s.settings.clone();
                self.entries = s
                    .messages
                    .iter()
                    .map(|m| {
                        if m.role == "user" {
                            Entry::User(m.content.clone())
                        } else {
                            Entry::Assistant {
                                text: m.content.clone(),
                                note: None,
                            }
                        }
                    })
                    .collect();
                self.status = format!("Loaded '{}'", s.title);
                self.session = s;
                self.scroll_to_bottom(terminal);
            }
            Err(e) => self.status = format!("load failed: {e}"),
        }
        self.focus = Focus::Input;
    }

    fn handle_command(&mut self, cmd: &str, terminal: &mut DefaultTerminal) {
        let (name, rest) = cmd.split_once(char::is_whitespace).unwrap_or((cmd, ""));
        let rest = rest.trim();
        match name.to_ascii_lowercase().as_str() {
            "new" => self.new_chat(),
            "sessions" => {
                self.save_session();
                self.sessions_list = session::list_sessions(&self.sessions_dir);
                self.sessions_selected = 0;
                self.focus = Focus::Sessions;
            }
            "settings" => {
                self.focus = Focus::Settings;
                self.settings_selected = 0;
            }
            "effort" => self.cmd_effort(rest),
            "temp" | "temperature" => self.cmd_temp(rest, terminal),
            "json" => self.cmd_json(rest, terminal),
            "stop" => self.cmd_stop(rest),
            "verify" => self.cmd_verify(rest, terminal),
            "personas" => self.cmd_personas(rest, terminal),
            "help" => self.entries.push(Entry::Info(HELP.to_string())),
            "quit" | "exit" => self.quit = true,
            "" => {}
            other => self
                .entries
                .push(Entry::Info(format!("unknown command /{other} — try /help"))),
        }
    }

    fn cmd_effort(&mut self, rest: &str) {
        if rest.is_empty() {
            self.status = format!("effort={} (usage: /effort none|low|medium|high)", self.settings.effort);
            return;
        }
        match Effort::parse(rest) {
            Ok(e) => {
                self.settings.effort = e;
                self.status = format!("effort set to {e}");
            }
            Err(e) => self.entries.push(Entry::Info(e)),
        }
    }

    /// Setting temperature used to be possible only through the settings
    /// panel, which made it easy to believe it had been set when it hadn't.
    fn cmd_temp(&mut self, rest: &str, terminal: &mut DefaultTerminal) {
        let rest = rest.trim();
        if rest.is_empty() {
            self.status = format!(
                "temperature={} (usage: /temp off | /temp 0.0-{} | /temp verify)",
                self.settings
                    .temperature
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "off".into()),
                config::TEMP_MAX
            );
            return;
        }
        if let Some(arg) = rest
            .strip_prefix("verify")
            .filter(|a| a.is_empty() || a.starts_with(char::is_whitespace))
        {
            self.cmd_temp_verify(arg.trim(), terminal);
            return;
        }
        if rest.eq_ignore_ascii_case("off") {
            self.settings.temperature = None;
            self.status = "temperature off (provider default)".into();
            return;
        }
        match rest.parse::<f32>().map_err(|_| format!("not a number: {rest}")).and_then(config::parse_temperature) {
            Ok(t) => {
                self.settings.temperature = Some(t);
                self.status = if self.settings.top_k.is_none() && self.settings.top_p.is_none() {
                    format!("temperature set to {t} — note: top_k/top_p are at the provider's defaults, which damp its effect (/settings)")
                } else {
                    format!("temperature set to {t}")
                };
            }
            Err(e) => self.entries.push(Entry::Info(e)),
        }
    }

    /// `/temp verify [runs] [prompt]` — the temperature counterpart of
    /// `/verify`. Chat replies are a bad instrument for judging a sampling
    /// knob (one question, one answer, no baseline), so this runs the
    /// controlled version: the same prompt N times at 0 and N times at the
    /// configured temperature, and prints the request body it sent.
    fn cmd_temp_verify(&mut self, rest: &str, terminal: &mut DefaultTerminal) {
        let (head, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let (runs, prompt) = match head.parse::<usize>() {
            Ok(n) => (n, tail.trim()),
            Err(_) => (verify::DEFAULT_TEMP_RUNS, rest),
        };
        let prompt = if prompt.is_empty() {
            verify::DEFAULT_TEMP_PROMPT.to_string()
        } else {
            prompt.to_string()
        };

        let ep = self.ep.clone();
        let settings = self.settings.clone();
        let label = format!("verifying temperature ({} calls)", runs * 2);
        let result = self.with_spinner(terminal, &label, move || {
            verify::run_temperature(&ep, &settings, &prompt, runs)
        });
        match result {
            Some(Ok(report)) => {
                self.status = report.headline();
                self.entries.push(Entry::Info(report.render()));
                self.scroll_to_bottom(terminal);
            }
            Some(Err(e)) => self.status = format!("temperature verify failed: {e}"),
            None => self.status = "temperature verify cancelled (Esc)".into(),
        }
    }

    fn cmd_stop(&mut self, rest: &str) {
        let (sub, arg) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        match sub {
            "add" if !arg.trim().is_empty() => {
                if self.settings.stop.len() >= 4 {
                    self.status = "at most 4 stop sequences are supported".into();
                    return;
                }
                self.settings.stop.push(config::unescape(arg.trim()));
                self.status = format!("stop sequences: {}", config::render_stops(&self.settings.stop));
            }
            "clear" => {
                self.settings.stop.clear();
                self.status = "stop sequences cleared".into();
            }
            _ => self.status = "usage: /stop add <seq> | /stop clear".into(),
        }
    }

    fn cmd_json(&mut self, rest: &str, terminal: &mut DefaultTerminal) {
        let (sub, arg) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let arg = arg.trim();
        match sub {
            "on" => {
                self.settings.json_mode.enabled = true;
                self.status = "json mode on".into();
            }
            "off" => {
                self.settings.json_mode.enabled = false;
                self.status = "json mode off".into();
            }
            "show" => self.entries.push(Entry::Info(format!(
                "current schema:\n{}",
                serde_json::to_string_pretty(&self.settings.json_mode.schema).unwrap_or_default()
            ))),
            "fields" if !arg.is_empty() => {
                let fields: Vec<String> = arg.split(',').map(|s| s.trim().to_string()).collect();
                self.settings.json_mode.schema = config::flat_string_schema(&fields);
                self.settings.json_mode.enabled = true;
                self.status = format!("json schema set: fields = {}", fields.join(", "));
            }
            "schema" if !arg.is_empty() => match serde_json::from_str::<Value>(arg) {
                Ok(schema) => match jsonschema::validator_for(&schema) {
                    Ok(_) => {
                        self.settings.json_mode.schema = schema;
                        self.settings.json_mode.enabled = true;
                        self.status = "json schema updated".into();
                    }
                    Err(e) => self.status = format!("not a valid JSON Schema: {e}"),
                },
                Err(e) => self.status = format!("not valid JSON: {e}"),
            },
            "edit" if !arg.is_empty() => self.json_edit(arg, terminal),
            _ => self.status =
                "usage: /json on|off|show|fields a,b,c|schema <json>|edit <instruction>".into(),
        }
    }

    /// Asks the model to rewrite the current schema per a natural-language
    /// instruction — the "the model can edit the JSON on request" path.
    fn json_edit(&mut self, instruction: &str, terminal: &mut DefaultTerminal) {
        let system = format!(
            "You maintain a JSON Schema (draft-like: type/properties/required/additionalProperties). \
             Current schema:\n{}\n\nRewrite it per the user's instruction. \
             Reply with ONLY the raw updated JSON Schema, no prose, no code fences.",
            serde_json::to_string(&self.settings.json_mode.schema).unwrap_or_default()
        );
        let history = vec![ChatMessage::user(instruction)];
        let mut plain = self.settings.without_stop_condition();
        plain.json_mode.enabled = false;
        plain.max_chars = None;
        let ep = self.ep.clone();
        let result = self.with_spinner(terminal, "updating schema", move || {
            api::chat(&ep, &plain, &system, &history, None)
        });
        match result {
            Some(Ok(outcome)) => {
                let text = strip_fences(outcome.text());
                match serde_json::from_str::<Value>(text)
                    .map_err(|e| e.to_string())
                    .and_then(|v| jsonschema::validator_for(&v).map(|_| v).map_err(|e| e.to_string()))
                {
                    Ok(schema) => {
                        self.settings.json_mode.schema = schema;
                        self.settings.json_mode.enabled = true;
                        self.status = "schema updated by the model".into();
                    }
                    Err(e) => self.status = format!("model reply was not a valid schema: {e}"),
                }
            }
            Some(Err(e)) => self.status = format!("schema edit failed: {e}"),
            None => self.status = "schema edit cancelled (Esc)".into(),
        }
    }

    fn cmd_verify(&mut self, rest: &str, terminal: &mut DefaultTerminal) {
        let prompt = if rest.is_empty() {
            verify::DEFAULT_PROMPT.to_string()
        } else {
            rest.to_string()
        };
        let ep = self.ep.clone();
        let settings = self.settings.clone();
        let result = self.with_spinner(terminal, "verifying stop condition (2 calls)", move || {
            verify::run(&ep, &settings, &prompt)
        });
        match result {
            Some(Ok(report)) => {
                self.status = if report.stop_condition_had_effect() {
                    "verify: stop condition CONFIRMED working".into()
                } else {
                    "verify: stop condition had NO effect — check budget_tokens/stop".into()
                };
                self.entries.push(Entry::Info(report.render()));
                self.scroll_to_bottom(terminal);
            }
            Some(Err(e)) => self.status = format!("verify failed: {e}"),
            None => self.status = "verify cancelled (Esc)".into(),
        }
    }

    /// Runs one question past several independent personas, one call at a
    /// time — never in parallel, since the provider handles concurrent
    /// requests from one client poorly. Each persona only ever sees the
    /// original question, not the other personas' answers, so the replies
    /// are genuinely independent takes rather than a single conversation
    /// that role-plays through several voices in one response.
    ///
    /// Usage: `/personas <question>` (default cast: physicist, philosopher,
    /// mathematician) or `/personas physicist,poet: <question>` for a custom
    /// cast.
    fn cmd_personas(&mut self, rest: &str, terminal: &mut DefaultTerminal) {
        if rest.trim().is_empty() {
            self.status = "usage: /personas [persona,persona,...:] <question>".into();
            return;
        }
        let (personas, question) = match rest.split_once(':') {
            Some((list, q)) if !q.trim().is_empty() => {
                let custom: Vec<String> =
                    list.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                if custom.is_empty() {
                    (default_personas(), rest.trim().to_string())
                } else {
                    (custom, q.trim().to_string())
                }
            }
            _ => (default_personas(), rest.trim().to_string()),
        };

        self.session.push_user(question.clone());
        self.entries.push(Entry::User(question.clone()));

        let n = personas.len();
        let mut cancelled = false;
        for (i, persona) in personas.iter().enumerate() {
            let system = format!(
                "You are answering strictly in character as an expert {persona}. Analyze the user's \
                 question using only the concepts, reasoning style and vocabulary of a {persona}, \
                 independent of any other perspective — don't mention or defer to other viewpoints. \
                 Be concise but substantive."
            );
            let ep = self.ep.clone();
            let settings = self.settings.clone();
            let history = vec![ChatMessage::user(question.clone())];
            let label = format!("thinking as {persona} ({}/{n})", i + 1);
            let result = self.with_spinner(terminal, &label, move || {
                api::chat(&ep, &settings, &system, &history, None)
            });
            match result {
                Some(Ok(outcome)) => {
                    let text = outcome.text().to_string();
                    self.session.push_assistant(format!("[{persona}] {text}"));
                    self.entries.push(Entry::Assistant {
                        text: format!("[{persona}]\n{text}"),
                        note: None,
                    });
                }
                Some(Err(e)) => self.entries.push(Entry::Info(format!("[{persona}] failed: {e}"))),
                None => {
                    cancelled = true;
                    break;
                }
            }
            self.scroll_to_bottom(terminal);
        }
        self.save_session();
        self.status = if cancelled {
            "personas cancelled (Esc) — earlier replies kept".into()
        } else {
            format!("ran {n} personas sequentially")
        };
    }

    /// Runs `f` on a background thread while animating `self.spinner` on the
    /// main thread so the transcript shows movement instead of freezing for
    /// the duration of a blocking network call. Returns `None` when the user
    /// pressed Esc (or Ctrl-Q, which also sets `quit`) while it was running —
    /// the worker is left to finish harmlessly in the background; its result
    /// is discarded.
    fn with_spinner<T, F>(&mut self, terminal: &mut DefaultTerminal, label: &str, f: F) -> Option<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(f());
        });
        let mut frame = 0usize;
        loop {
            self.spinner = Some((label.to_string(), SPINNER_FRAMES[frame % SPINNER_FRAMES.len()]));
            if self.follow {
                self.scroll_to_bottom(terminal);
            }
            let _ = terminal.draw(|f| self.draw(f));
            if self.poll_cancel_keys() {
                self.spinner = None;
                return None;
            }
            match rx.recv_timeout(Duration::from_millis(110)) {
                Ok(v) => {
                    self.spinner = None;
                    return Some(v);
                }
                Err(RecvTimeoutError::Timeout) => frame = frame.wrapping_add(1),
                Err(RecvTimeoutError::Disconnected) => {
                    self.spinner = None;
                    panic!("spinner worker thread ended without a result");
                }
            }
        }
    }

    /// Non-blocking drain of pending key events while a request is in flight
    /// (the main loop is otherwise stuck polling the worker channel and no
    /// key would ever be seen). Esc asks to stop the current generation;
    /// Ctrl-Q stops it and quits. Any other key is ignored.
    fn poll_cancel_keys(&mut self) -> bool {
        let mut stop = false;
        while event::poll(Duration::ZERO).unwrap_or(false) {
            let Ok(event::Event::Key(key)) = event::read() else { continue };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
                self.quit = true;
                stop = true;
            } else if key.code == KeyCode::Esc {
                stop = true;
            }
        }
        stop
    }

    fn adjust_setting(&mut self, delta: i32) {
        match self.settings_selected {
            0 => self.settings.effort = self.settings.effort.cycle(delta),
            1 => self.settings.json_mode.enabled = !self.settings.json_mode.enabled,
            2 => {
                self.settings.max_chars = cycle_choice(MAX_CHARS_CHOICES, self.settings.max_chars, delta)
            }
            3 => {
                self.settings.budget_tokens =
                    cycle_choice(BUDGET_CHOICES, self.settings.budget_tokens, delta)
            }
            4 => {
                let current = STOP_PRESETS
                    .iter()
                    .position(|p| p.iter().map(|s| s.to_string()).collect::<Vec<_>>() == self.settings.stop)
                    .unwrap_or(0) as i32;
                let n = STOP_PRESETS.len() as i32;
                let next = STOP_PRESETS[(current + delta).rem_euclid(n) as usize];
                self.settings.stop = next.iter().map(|s| s.to_string()).collect();
            }
            5 => self.settings.temperature = cycle_float(TEMP_CHOICES, self.settings.temperature, delta),
            6 => self.settings.top_p = cycle_float(TOP_P_CHOICES, self.settings.top_p, delta),
            7 => self.settings.top_k = cycle_choice(TOP_K_CHOICES, self.settings.top_k, delta),
            _ => {}
        }
    }

    fn send_message(&mut self, question: String, terminal: &mut DefaultTerminal) {
        self.session.push_user(question.clone());
        self.entries.push(Entry::User(question));
        self.follow = true;
        let history = self.session.history();

        // JSON mode stays on the blocking path: streamed fragments of a JSON
        // object aren't valid JSON until the last token, so there's nothing
        // meaningful to render live, and `render_json_reply` needs the whole
        // body to flatten anyway.
        if self.settings.json_mode.enabled {
            let schema = self.settings.json_mode.schema.clone();
            let ep = self.ep.clone();
            let settings = self.settings.clone();
            let hist = history.clone();
            let result = self.with_spinner(terminal, "model is thinking", move || {
                api::chat(&ep, &settings, "", &hist, Some(&schema))
            });
            match result {
                Some(Ok(outcome)) => self.finish_reply(outcome, terminal),
                Some(Err(e)) => {
                    self.session.messages.pop();
                    self.status = format!("request failed: {e}");
                }
                None => {
                    self.session.messages.pop();
                    self.status = "generation cancelled (Esc)".into();
                }
            }
            return;
        }

        self.entries.push(Entry::Assistant { text: String::new(), note: None });
        let idx = self.entries.len() - 1;
        self.status = "generating…".into();

        // Runs the connect-and-read loop on a background thread so the main
        // loop can keep animating the spinner during the (sometimes long)
        // silent stretch spent inside hidden `reasoning_content` deltas,
        // instead of blocking on the socket read with a frozen screen.
        // `cancel` lets an Esc in the main loop stop the read loop at the
        // next SSE line, which drops the stream and closes the connection.
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let ep = self.ep.clone();
        let settings = self.settings.clone();
        let cancel_worker = cancel.clone();
        thread::spawn(move || {
            let mut stream =
                match api::chat_stream(&ep, &settings, "", &history, None, Some(cancel_worker)) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = tx.send(StreamEvent::Done(Err(e)));
                        return;
                    }
                };
            loop {
                match stream.next_chunk() {
                    Ok(Some(piece)) => {
                        if tx.send(StreamEvent::Piece(piece)).is_err() {
                            return;
                        }
                    }
                    Ok(None) => {
                        let _ = tx.send(StreamEvent::Done(Ok(stream.into_outcome())));
                        return;
                    }
                    Err(e) => {
                        // Keep whatever text already streamed in rather than
                        // discarding it — the user watched it arrive, so it
                        // stays in the transcript (and in session history) with
                        // a note explaining the cut.
                        let mut outcome = stream.into_outcome();
                        let interrupted = format!("stream interrupted: {e}");
                        outcome.finish_reason = Some(
                            outcome
                                .finish_reason
                                .map_or(interrupted.clone(), |fr| format!("{fr}, {interrupted}")),
                        );
                        let _ = tx.send(StreamEvent::Done(Ok(outcome)));
                        return;
                    }
                }
            }
        });

        let mut frame = 0usize;
        loop {
            self.spinner = Some(("model is thinking".into(), SPINNER_FRAMES[frame % SPINNER_FRAMES.len()]));
            if self.follow {
                self.scroll_to_bottom(terminal);
            }
            let _ = terminal.draw(|f| self.draw(f));
            if self.poll_cancel_keys() {
                // Stop waiting for the worker: raise the flag so its read
                // loop ends at the next SSE line (dropping the connection),
                // then finalize whatever text already made it to the screen.
                // Dropping `rx` on the way out makes the worker exit too.
                self.spinner = None;
                cancel.store(true, Ordering::Relaxed);
                self.finish_cancelled(idx, terminal);
                return;
            }
            match rx.recv_timeout(Duration::from_millis(110)) {
                Ok(StreamEvent::Piece(piece)) => {
                    if let Entry::Assistant { text, .. } = &mut self.entries[idx] {
                        text.push_str(&piece);
                    }
                }
                Ok(StreamEvent::Done(result)) => {
                    self.spinner = None;
                    match result {
                        Ok(outcome) => self.finish_reply_at(idx, outcome, terminal),
                        Err(e) => {
                            self.session.messages.pop();
                            self.entries.remove(idx);
                            self.status = format!("request failed: {e}");
                        }
                    }
                    return;
                }
                Err(RecvTimeoutError::Timeout) => frame = frame.wrapping_add(1),
                Err(RecvTimeoutError::Disconnected) => {
                    self.spinner = None;
                    self.session.messages.pop();
                    self.entries.remove(idx);
                    self.status = "request failed: worker thread ended unexpectedly".into();
                    return;
                }
            }
        }
    }

    /// Tail for a reply the user stopped with Esc: keep whatever text
    /// already streamed in (subject to the same `max_chars` cap as a normal
    /// reply) with a note marking the cut, or drop the half-exchange entirely
    /// if nothing visible ever arrived.
    fn finish_cancelled(&mut self, idx: usize, terminal: &mut DefaultTerminal) {
        let partial = match self.entries.get(idx) {
            Some(Entry::Assistant { text, .. }) if !text.trim().is_empty() => text.clone(),
            _ => String::new(),
        };
        if partial.is_empty() {
            self.session.messages.pop();
            self.entries.remove(idx);
            self.status = "generation stopped (Esc) — nothing was generated yet".into();
        } else {
            let (capped, was_cut) = api::enforce_max_chars(&partial, self.settings.max_chars);
            let mut note = "stopped by Esc".to_string();
            if was_cut {
                note = format!("{note} · truncated to {} chars", self.settings.max_chars.unwrap_or(0));
            }
            if let Some(Entry::Assistant { text, note: n }) = self.entries.get_mut(idx) {
                *text = capped.clone();
                *n = Some(note);
            }
            self.session.push_assistant(capped);
            self.status = "generation stopped by Esc — partial reply kept".into();
        }
        self.save_session();
        if self.follow {
            self.scroll_to_bottom(terminal);
        }
    }

    /// Shared tail of both the streaming and blocking reply paths: enforce
    /// `max_chars`, persist to the session, build the status/note text, and
    /// write the final text into `entries[idx]`.
    fn finish_reply_at(&mut self, idx: usize, outcome: api::Outcome, terminal: &mut DefaultTerminal) {
        let raw_text = outcome.text();
        let (display, parse_note) = if self.settings.json_mode.enabled {
            render::render_json_reply(raw_text)
        } else {
            (raw_text.to_string(), None)
        };
        let (capped, was_cut) = api::enforce_max_chars(&display, self.settings.max_chars);

        self.session.push_assistant(capped.clone());
        let mut note_parts = Vec::new();
        if was_cut {
            note_parts.push(format!(
                "truncated to {} chars",
                self.settings.max_chars.unwrap_or(0)
            ));
        }
        if outcome.truncated() {
            note_parts.push("cut by token budget (finish_reason=length)".into());
        }
        if outcome.stopped_by_sequence() && !self.settings.stop.is_empty() {
            note_parts.push("stopped on a stop sequence".into());
        }
        if let Some(e) = parse_note {
            note_parts.push(e);
        }
        if let Some(r) = outcome.reasoning.as_deref().filter(|s| !s.trim().is_empty()) {
            note_parts.push(format!("reasoning: {} chars", r.trim().chars().count()));
        }
        let note = (!note_parts.is_empty()).then(|| note_parts.join(" · "));
        if let Some(Entry::Assistant { text, note: n }) = self.entries.get_mut(idx) {
            *text = capped;
            *n = note;
        }
        self.status = format!(
            "finish={} · tokens: prompt={} completion={} (reasoning={}) · {}ms",
            outcome.finish_reason.as_deref().unwrap_or("?"),
            outcome.usage.prompt_tokens,
            outcome.usage.completion_tokens,
            outcome.usage.reasoning_tokens,
            outcome.latency_ms
        );
        self.save_session();
        if self.follow {
            self.scroll_to_bottom(terminal);
        }
    }

    /// Blocking-path variant: no live entry exists yet, so append one first.
    fn finish_reply(&mut self, outcome: api::Outcome, terminal: &mut DefaultTerminal) {
        self.entries.push(Entry::Assistant { text: String::new(), note: None });
        let idx = self.entries.len() - 1;
        self.finish_reply_at(idx, outcome, terminal);
    }

    fn draw(&self, f: &mut Frame) {
        let width = f.area().width;
        let input_height = self.input_box_height(width);
        let [header, body, input, footer] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(input_height),
            Constraint::Length(2),
        ])
        .areas(f.area());
        self.draw_header(f, header);
        self.draw_transcript(f, body);
        self.draw_input(f, input, width);
        self.draw_footer(f, footer);

        if self.focus == Focus::Settings {
            self.draw_settings_overlay(f, body);
        } else if self.focus == Focus::Sessions {
            self.draw_sessions_overlay(f, body);
        } else if self.focus == Focus::Input && self.command_popup_active() {
            self.draw_command_popup(f, body);
        }
    }

    /// Input box: renders only the wrapped lines visible inside the (at most
    /// 4-row) window and places the text cursor at the cursor offset. The
    /// same `wrap_input` output drives layout, rendering and cursor math, so
    /// they cannot drift out of sync.
    fn draw_input(&self, f: &mut Frame, area: Rect, width: u16) {
        let input_style = if self.focus == Focus::Input {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        };
        let lines = input_lines(&self.input, width);
        let view = (area.height.saturating_sub(2)) as usize;
        let scroll = self
            .input_scroll
            .min(lines.len().saturating_sub(view.min(lines.len())));
        let visible: Vec<Line> = lines
            .iter()
            .skip(scroll)
            .take(view)
            .map(|(_, text)| Line::raw(text.clone()))
            .collect();
        f.render_widget(
            Paragraph::new(visible)
                .style(input_style)
                .block(Block::bordered().title(" message — /help for commands ")),
            area,
        );
        if self.focus == Focus::Input {
            let (row, col) = cursor_visual_pos(&lines, self.cursor);
            if row >= scroll && row < scroll + view {
                let cx = area.x + 1 + (col as u16).min(area.width.saturating_sub(3));
                let cy = area.y + 1 + (row - scroll) as u16;
                f.set_cursor_position((cx, cy));
            }
        }
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    " ask chat ",
                    Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::raw(&self.session.title),
                Span::raw("   "),
                Span::styled(self.settings.summary(), Style::default().fg(Color::DarkGray)),
            ]))
            .block(Block::bordered()),
            area,
        );
    }

    fn transcript_text(&self) -> String {
        let mut body = String::new();
        for entry in &self.entries {
            match entry {
                Entry::User(text) => body.push_str(&format!("YOU\n{text}\n\n")),
                Entry::Assistant { text, note } => {
                    body.push_str("ASSISTANT\n");
                    body.push_str(text);
                    body.push('\n');
                    if let Some(note) = note {
                        body.push_str(&format!("[{note}]\n"));
                    }
                    body.push('\n');
                }
                Entry::Info(text) => body.push_str(&format!("--- {text} ---\n\n")),
            }
        }
        if body.is_empty() {
            body = "No messages yet — say something.".into();
        }
        if let Some((label, frame)) = &self.spinner {
            body.push_str(&format!("{frame} {label}…\n"));
        }
        body
    }

    /// Draw-time clamp: growing the input box shrinks the transcript area, so
    /// a stale offset could exceed `max_scroll` — and `Paragraph::scroll`
    /// clips rather than clamps, which would render a blank pane.
    fn draw_transcript(&self, f: &mut Frame, area: Rect) {
        let max = self.max_scroll_for(f.area().width, f.area().height);
        f.render_widget(
            Paragraph::new(self.transcript_text())
                .wrap(Wrap { trim: false })
                .scroll((self.scroll.min(max), 0))
                .block(Block::bordered().title(" conversation ")),
            area,
        );
    }

    /// The last-page scroll offset for the wrapped transcript. `Paragraph::scroll`
    /// clips rather than clamps, so any offset past this renders a blank pane —
    /// every manual or automatic scroll must be capped to this value.
    fn max_scroll(&self, terminal: &DefaultTerminal) -> u16 {
        let Ok(size) = terminal.size() else { return self.scroll };
        self.max_scroll_for(size.width, size.height)
    }

    /// The last-page scroll offset for given frame dimensions (kept separate
    /// from `max_scroll` so `draw` can clamp without a terminal). Mirrors
    /// the vertical layout in `draw`: header(3) + input + footer(2).
    fn max_scroll_for(&self, width: u16, height: u16) -> u16 {
        let body_height = height.saturating_sub(3 + self.input_box_height(width) + 2);
        let inner_width = width.saturating_sub(2); // block borders
        let inner_height = body_height.saturating_sub(2); // block borders
        let total_lines =
            Paragraph::new(self.transcript_text()).wrap(Wrap { trim: false }).line_count(inner_width) as u16;
        total_lines.saturating_sub(inner_height)
    }

    /// Scrolls to the true bottom of the wrapped transcript (see `max_scroll`)
    /// and re-pins the view to incoming content.
    fn scroll_to_bottom(&mut self, terminal: &DefaultTerminal) {
        self.scroll = self.max_scroll(terminal);
        self.follow = true;
    }

    fn draw_settings_overlay(&self, f: &mut Frame, area: Rect) {
        let popup = centered(area, 60, SETTINGS_ROWS.len() as u16 + 2);
        let items = SETTINGS_ROWS
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let marker = if i == self.settings_selected { "▸ " } else { "  " };
                ListItem::new(Line::from(vec![
                    Span::raw(format!("{marker}{name:<14}")),
                    Span::styled(self.setting_value(i), Style::default().fg(Color::Green)),
                ]))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default();
        state.select(Some(self.settings_selected));
        f.render_widget(ratatui::widgets::Clear, popup);
        f.render_stateful_widget(
            List::new(items).block(Block::bordered().title(" settings — ↑↓ select, ←→ change, Esc close ")),
            popup,
            &mut state,
        );
    }

    fn setting_value(&self, row: usize) -> String {
        match row {
            0 => self.settings.effort.to_string(),
            1 => if self.settings.json_mode.enabled { "on" } else { "off" }.into(),
            2 => self.settings.max_chars.map(|n| n.to_string()).unwrap_or_else(|| "off".into()),
            3 => self.settings.budget_tokens.map(|n| n.to_string()).unwrap_or_else(|| "off".into()),
            4 => config::render_stops(&self.settings.stop),
            5 => self.settings.temperature.map(|t| t.to_string()).unwrap_or_else(|| "off".into()),
            6 => self
                .settings
                .top_p
                .map(|p| p.to_string())
                .unwrap_or_else(|| "provider default".into()),
            7 => self
                .settings
                .top_k
                .map(config::render_top_k)
                .unwrap_or_else(|| "provider default".into()),
            _ => String::new(),
        }
    }

    fn draw_sessions_overlay(&self, f: &mut Frame, area: Rect) {
        let popup = centered(area, 70, 12.min(area.height.saturating_sub(2)).max(4));
        let items: Vec<ListItem> = if self.sessions_list.is_empty() {
            vec![ListItem::new("(no saved sessions yet)")]
        } else {
            self.sessions_list
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let marker = if i == self.sessions_selected { "▸ " } else { "  " };
                    ListItem::new(format!("{marker}{:<32} {} msgs", s.title, s.message_count))
                })
                .collect()
        };
        let mut state = ListState::default();
        state.select(Some(self.sessions_selected));
        f.render_widget(ratatui::widgets::Clear, popup);
        f.render_stateful_widget(
            List::new(items)
                .block(Block::bordered().title(" sessions — ↑↓ select, Enter open, d delete, Esc close ")),
            popup,
            &mut state,
        );
        if let Some(idx) = self.sessions_pending_delete {
            if let Some(s) = self.sessions_list.get(idx) {
                self.draw_delete_confirm(f, area, &s.title);
            }
        }
    }

    fn draw_delete_confirm(&self, f: &mut Frame, area: Rect, title: &str) {
        let msg = format!("Delete '{title}'? y/n");
        let width = (msg.chars().count() as u16 + 4).min(area.width.saturating_sub(2)).max(20);
        let popup = centered(area, width, 3);
        f.render_widget(ratatui::widgets::Clear, popup);
        f.render_widget(
            Paragraph::new(msg)
                .style(Style::default().fg(Color::Red))
                .block(Block::bordered().title(" confirm delete ")),
            popup,
        );
    }

    /// Anchored to the bottom of the transcript area, directly above the
    /// input box, so it reads like a dropdown under the cursor.
    fn draw_command_popup(&self, f: &mut Frame, body: Rect) {
        let cmds = self.filtered_commands();
        let selected = self.cmd_selected.min(cmds.len().saturating_sub(1));
        let height = (cmds.len() as u16 + 2).min(body.height);
        let width = 40.min(body.width);
        let popup = Rect {
            x: body.x,
            y: body.y + body.height.saturating_sub(height),
            width,
            height,
        };
        let items: Vec<ListItem> = cmds
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let marker = if i == selected { "▸ " } else { "  " };
                ListItem::new(format!("{marker}/{name}"))
            })
            .collect();
        let mut state = ListState::default();
        state.select(Some(selected));
        f.render_widget(ratatui::widgets::Clear, popup);
        f.render_stateful_widget(
            List::new(items).block(Block::bordered().title(" commands — ↑↓ select, → apply, ← close ")),
            popup,
            &mut state,
        );
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let keys = if self.spinner.is_some() {
            "Esc stop generation · Ctrl-Q quit"
        } else {
            "Enter send/run · Tab settings · Ctrl-N new · / Commands · Esc quit"
        };
        f.render_widget(
            Paragraph::new(vec![
                Line::styled(self.status.clone(), Style::default().fg(Color::Cyan)),
                Line::styled(keys, Style::default().fg(Color::DarkGray)),
            ]),
            area,
        );
    }
}

const HELP: &str = "\
/new                      start a new chat session
/sessions                 list and switch between saved sessions
/effort [none|low|medium|high]   get or set reasoning effort
/json on|off              toggle structured JSON output
/json fields a,b,c        set a flat schema with these string fields
/json schema <json>       set a full JSON Schema
/json edit <instruction>  ask the model to rewrite the schema
/json show                print the active schema
/temp [off|0.0-2.0]       get or set sampling temperature (2.0 is the provider's max)
/temp verify [n] [prompt]  prove temperature changes the output (n samples at 0 vs at yours)
/stop add <seq>           add a stop sequence (max 4)
/stop clear               clear stop sequences
/verify [prompt]          prove the stop condition changes the output
/personas <question>      ask physicist/philosopher/mathematician, one call each, in sequence
/personas a,b,c: <question>   same, with your own cast instead of the default three
/settings                 open the settings panel (Tab does the same)
/quit                     exit
Esc while generating      stop the current generation (partial reply is kept)
Ctrl-Q                    quit, even mid-generation";

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2)).max(10);
    let height = height.min(area.height.saturating_sub(2)).max(3);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

fn cycle_choice<T: PartialEq + Copy>(choices: &[T], current: T, delta: i32) -> T {
    let i = choices.iter().position(|c| *c == current).unwrap_or(0) as i32;
    let n = choices.len() as i32;
    choices[(i + delta).rem_euclid(n) as usize]
}

/// `f32` doesn't implement `Eq`, so the float rows (temperature, top_p) get
/// their own cycler over a fixed choice set (bitwise comparison is fine —
/// these are exact constants, not computed values).
fn cycle_float(choices: &[Option<f32>], current: Option<f32>, delta: i32) -> Option<f32> {
    let i = choices
        .iter()
        .position(|c| c.map(f32::to_bits) == current.map(f32::to_bits))
        .unwrap_or(0) as i32;
    let n = choices.len() as i32;
    choices[(i + delta).rem_euclid(n) as usize]
}

/// Byte offset of the char at `char_idx` (`s.len()` when the index is at/past
/// the end) — bridges the char-offset cursor and the byte-offset `String` API.
fn byte_pos(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map(|(i, _)| i).unwrap_or(s.len())
}

/// Pasted newlines arrive as `\n` from most terminals but as `\r` (tmux)
/// or `\r\n` from others; fold all three forms onto `\n` so the text wraps
/// into real input lines instead of smearing across the row via CR.
fn normalize_paste(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Inner text width of the input box for a frame of `width` columns.
fn input_width(width: u16) -> usize {
    width.saturating_sub(2).max(1) as usize
}

/// The input text wrapped into visual lines for a frame of `width` columns.
fn input_lines(input: &str, width: u16) -> Vec<(usize, String)> {
    wrap_input(input, input_width(width))
}

/// Greedy word-wrap of the input into visual lines of at most `width`
/// columns. Returns one entry per visual line: `(start, text)` where `start`
/// is the char offset of the line's first char, so a char-offset cursor can
/// be mapped to a (row, col) on screen. Rendering uses these same lines, so
/// the mapping never drifts out of sync with what the user sees. Words
/// longer than `width` are hard-split; explicit newlines always break.
fn wrap_input(input: &str, width: usize) -> Vec<(usize, String)> {
    let width = width.max(1);
    let chars: Vec<char> = input.chars().collect();
    let n = chars.len();
    let mut lines: Vec<(usize, String)> = Vec::new();
    let mut cur = String::new();
    let mut cur_start = 0usize;
    let mut cur_w = 0usize;
    let mut spaces = 0usize; // spaces between the last committed word and the next
    let mut word = String::new();
    let mut word_start = 0usize;
    let mut i = 0usize;
    while i <= n {
        let c = chars.get(i).copied();
        if let Some(ch) = c {
            if ch != ' ' && ch != '\n' {
                if word.is_empty() {
                    word_start = i;
                }
                word.push(ch);
                i += 1;
                continue;
            }
        }
        // Word boundary (space, newline, or end of input): commit the word.
        if !word.is_empty() {
            let wlen = word.chars().count();
            let fits = if cur_w == 0 { spaces + wlen <= width } else { cur_w + spaces + wlen <= width };
            if fits {
                if cur_w == 0 {
                    cur_start = word_start.saturating_sub(spaces);
                }
                cur.extend(std::iter::repeat_n(' ', spaces));
                cur.push_str(&word);
                cur_w += spaces + wlen;
            } else {
                // Wrap: the word starts a new line (the buffered spaces at the
                // break point are not rendered). Nothing to emit when the
                // current line is still empty (word alone overflows it).
                if !cur.is_empty() {
                    lines.push((cur_start, std::mem::take(&mut cur)));
                }
                if wlen <= width {
                    cur_start = word_start;
                    cur.push_str(&word);
                    cur_w = wlen;
                } else {
                    // Word longer than the whole line: hard-split it.
                    let mut chunk_start = word_start;
                    let mut chunk_w = 0usize;
                    for (k, ch) in word.chars().enumerate() {
                        if chunk_w == width {
                            lines.push((chunk_start, std::mem::take(&mut cur)));
                            chunk_start = word_start + k;
                            chunk_w = 0;
                        }
                        cur.push(ch);
                        chunk_w += 1;
                    }
                    cur_start = chunk_start;
                    cur_w = chunk_w;
                }
            }
            word.clear();
            spaces = 0;
        }
        match c {
            Some(' ') => {
                spaces += 1;
                i += 1;
            }
            Some('\n') => {
                lines.push((cur_start, std::mem::take(&mut cur)));
                cur_w = 0;
                cur_start = i + 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    lines.push((cur_start, cur));
    if lines.is_empty() {
        lines.push((0, String::new()));
    }
    lines
}

/// Maps a char-offset cursor to its (visual row, column) in `lines` produced
/// by `wrap_input` (column clamped to the line's length).
fn cursor_visual_pos(lines: &[(usize, String)], cursor: usize) -> (usize, usize) {
    let mut row = 0;
    for (i, (start, _)) in lines.iter().enumerate() {
        if *start <= cursor {
            row = i;
        } else {
            break;
        }
    }
    let (start, text) = &lines[row];
    (row, (cursor - start).min(text.chars().count()))
}

/// Moves the cursor `delta` visual lines from its current row, keeping the
/// column where possible. Returns `None` when the cursor is already on the
/// first/last line — the caller turns that case into transcript scrolling.
fn move_cursor_line(lines: &[(usize, String)], cursor: usize, delta: i32) -> Option<usize> {
    let (row, col) = cursor_visual_pos(lines, cursor);
    let target = row as i32 + delta;
    if target < 0 || target >= lines.len() as i32 {
        return None;
    }
    let (start, text) = &lines[target as usize];
    Some(*start + col.min(text.chars().count()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_mode_default_is_a_flat_schema() {
        let jm = config::JsonMode::default();
        assert!(!jm.enabled);
        assert_eq!(jm.schema["type"], "object");
    }

    #[test]
    fn temperature_choices_stay_inside_the_providers_range() {
        for t in TEMP_CHOICES.iter().flatten() {
            assert!(config::parse_temperature(*t).is_ok(), "{t} out of range");
        }
        // Stepping back from "off" lands on the ceiling, so it is reachable.
        assert_eq!(cycle_float(TEMP_CHOICES, None, -1), Some(config::TEMP_MAX));
        assert_eq!(cycle_float(TEMP_CHOICES, None, 1), Some(0.0));
    }

    #[test]
    fn sampling_rows_render_and_cycle() {
        let mut app = App::new(api::Endpoint::dummy(), config::Settings::default());
        let temp_row = SETTINGS_ROWS.iter().position(|r| *r == "temperature").unwrap();
        let top_p_row = SETTINGS_ROWS.iter().position(|r| *r == "top_p").unwrap();
        let top_k_row = SETTINGS_ROWS.iter().position(|r| *r == "top_k").unwrap();
        assert_eq!(app.setting_value(temp_row), "off");
        assert_eq!(app.setting_value(top_p_row), "provider default");
        assert_eq!(app.setting_value(top_k_row), "provider default");

        app.settings_selected = top_k_row;
        app.adjust_setting(-1); // one step back from "off" is the "full" end
        assert_eq!(app.settings.top_k, Some(-1));
        assert_eq!(app.setting_value(top_k_row), "full");
    }

    #[test]
    fn cycle_choice_wraps_both_directions() {
        let choices = [None, Some(1u32), Some(2)];
        assert_eq!(cycle_choice(&choices, None, 1), Some(1));
        assert_eq!(cycle_choice(&choices, None, -1), Some(2));
    }
    #[test]
    fn wrap_short_text_is_one_line() {
        assert_eq!(wrap_input("hello", 10), vec![(0, "hello".to_string())]);
        assert_eq!(wrap_input("", 10), vec![(0, String::new())]);
    }

    #[test]
    fn wrap_breaks_at_word_boundary() {
        let lines = wrap_input("aaa bbb ccc", 7);
        assert_eq!(lines, vec![(0, "aaa bbb".to_string()), (8, "ccc".to_string())]);
    }

    #[test]
    fn wrap_hard_splits_long_words() {
        let lines = wrap_input("abcdefgh", 3);
        assert_eq!(
            lines,
            vec![(0, "abc".to_string()), (3, "def".to_string()), (6, "gh".to_string())]
        );
    }

    #[test]
    fn wrap_respects_explicit_newlines() {
        let lines = wrap_input("ab\ncd\n", 10);
        assert_eq!(
            lines,
            vec![(0, "ab".to_string()), (3, "cd".to_string()), (6, String::new())]
        );
    }

    #[test]
    fn cursor_maps_to_visual_row_and_column() {
        let lines = wrap_input("aaa bbb ccc", 7);
        assert_eq!(cursor_visual_pos(&lines, 0), (0, 0));
        assert_eq!(cursor_visual_pos(&lines, 7), (0, 7)); // end of first row
        assert_eq!(cursor_visual_pos(&lines, 8), (1, 0)); // start of wrapped word
        assert_eq!(cursor_visual_pos(&lines, 11), (1, 3)); // end of input
    }

    #[test]
    fn cursor_after_shift_enter_starts_new_row() {
        // Simulates typing "ab" then Shift+Enter: cursor lands at the start
        // of the freshly created (empty) visual line.
        let lines = wrap_input("ab\n", 10);
        assert_eq!(cursor_visual_pos(&lines, 3), (1, 0));
    }

    #[test]
    fn move_cursor_line_keeps_column_and_clamps() {
        // Column is preserved when the target line is long enough…
        let lines = wrap_input("aaaa\nbbbb", 10);
        assert_eq!(move_cursor_line(&lines, 3, 1), Some(8)); // col 3 on row 1
        // …and clamped to a shorter target line.
        let lines = wrap_input("aaaa\nbb", 10);
        assert_eq!(move_cursor_line(&lines, 3, 1), Some(7)); // col 2 (clamped)
    }

    #[test]
    fn move_cursor_line_returns_none_at_edges() {
        let lines = wrap_input("aaaa\nbbbb", 10);
        assert_eq!(move_cursor_line(&lines, 2, -1), None); // on first row
        assert_eq!(move_cursor_line(&lines, 7, 1), None); // on last row
        // Single-line input: both directions hit an edge → transcript scrolls.
        let lines = wrap_input("single", 10);
        assert_eq!(move_cursor_line(&lines, 3, -1), None);
        assert_eq!(move_cursor_line(&lines, 3, 1), None);
    }

    #[test]
    fn byte_pos_tracks_char_boundaries() {
        let s = "aé日 b";
        assert_eq!(byte_pos(s, 0), 0);
        assert_eq!(byte_pos(s, 2), "aé".len());
        assert_eq!(byte_pos(s, 3), "aé日".len());
        assert_eq!(byte_pos(s, 99), s.len());
    }

    #[test]
    fn normalize_paste_folds_cr_and_crlf_onto_lf() {
        assert_eq!(normalize_paste("a\nb"), "a\nb");
        assert_eq!(normalize_paste("a\rb"), "a\nb");
        assert_eq!(normalize_paste("a\r\nb"), "a\nb");
        assert_eq!(normalize_paste("no breaks"), "no breaks");
    }
}
