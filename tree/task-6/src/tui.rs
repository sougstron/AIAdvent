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
use ratatui::widgets::{Block, BorderType, List, ListItem, ListState, Padding, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use serde_json::Value;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::agent::Agent;
use crate::api::{self, ChatMessage, Endpoint, Outcome};
use crate::auth::{self, CheckResult, Provider};
use crate::config::{self, Effort, Res, Settings};
use crate::render::{self, strip_fences};
use crate::session::{self, Session, SessionSummary};
use crate::verify;

const MAX_CHARS_CHOICES: &[Option<usize>] =
    &[None, Some(100), Some(250), Some(500), Some(1000), Some(2000)];
const BUDGET_CHOICES: &[Option<u32>] =
    &[None, Some(32), Some(64), Some(128), Some(256), Some(512), Some(1024), Some(4096)];
const STOP_PRESETS: &[&[&str]] = &[&[], &["\n\n"], &["\n---\n"], &["\n\n\n"]];
/// Up to the documented z.ai ceiling (`config::TEMP_MAX` = 1.0).
const TEMP_CHOICES: &[Option<f32>] = &[
    None,
    Some(0.0),
    Some(0.3),
    Some(0.7),
    Some(1.0),
];
const TOP_P_CHOICES: &[Option<f32>] = &[None, Some(0.5), Some(0.8), Some(0.95), Some(1.0)];
const TOP_K_CHOICES: &[Option<i32>] = &[None, Some(20), Some(50), Some(100), Some(-1)];
const SETTINGS_ROWS: &[&str] = &[
    "model",
    "effort",
    "json mode",
    "max_chars",
    "budget_tokens",
    "stop",
    "temperature",
    "top_p",
    "top_k",
    "system_prompt",
];
/// Slash commands offered by the input popup, kept in alphabetical order
/// since that's the order the popup lists them in.
const COMMANDS: &[&str] = &[
    "context", "effort", "help", "json", "login", "max-tokens", "model", "new", "personas", "quit",
    "rename", "sessions", "settings", "stop", "system", "temp", "top-k", "top-p", "verify",
];
/// Cycled while a background call is in flight — drawn inline in the
/// transcript instead of a full-screen "working" overlay.
const SPINNER_FRAMES: &[&str] =
    &["\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}", "\u{2827}", "\u{2807}", "\u{280f}"];

/// The whole palette. One accent for anything the user acts on (focus,
/// prompt, the assistant's voice) and one muted tone for anything the user
/// only glances at; everything else keeps the terminal's own foreground, so
/// the app reads as text rather than as a form and inherits the user's theme.
const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;

fn accent() -> Style {
    Style::default().fg(ACCENT)
}

fn muted() -> Style {
    Style::default().fg(MUTED)
}

/// Every overlay wears the same frame: a rounded muted border, an accent
/// title on top, and its key hints dimmed along the bottom edge instead of
/// crowding the title line.
fn panel(title: &str, hints: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(muted())
        .title(Span::styled(format!(" {title} "), accent().add_modifier(Modifier::BOLD)))
        .title_bottom(Span::styled(format!(" {hints} "), muted()))
}

/// Selection inside an overlay is carried by the \u{25b8} marker; this only adds the
/// weight, so a row that already colours itself (an applied model) keeps it.
fn selected_row() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}
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

pub fn run(settings: Settings, loaded: Option<Session>) -> Res<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("interactive chat requires a terminal".into());
    }
    // Keyless boot: a missing key must not keep the app from starting —
    // the whole point of /login is to connect one from inside. The agent
    // comes up on a provably unusable endpoint until a key is connected.
    // If *some* provider is connected but not the one that owns the current
    // model, we still boot — the picker will offer that provider's ids.
    let (mut agent, boot) = match Endpoint::for_model(&settings.model) {
        Ok(ep) => (Agent::with_endpoint(ep, settings.clone()), None),
        Err(e) => (
            Agent::with_endpoint(Endpoint::unusable(), settings.clone()),
            if auth::connected_providers().is_empty() {
                Some(format!("no API key yet — connect one below ({e})"))
            } else {
                None
            },
        ),
    };
    let settings = if let Some(ref session) = loaded {
        agent.resume(session);
        session.settings.clone()
    } else {
        settings
    };
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
    let mut app = App::new(agent, settings, loaded);
    if let Some(msg) = boot {
        app.start_keyless(&msg);
    } else {
        app.warn_if_model_unavailable();
    }
    let result = app.event_loop(&mut terminal);
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
    Model,
    Login,
}

struct App {
    agent: Agent,
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
    login_selected: usize,
    login_rows: Vec<auth::ProviderStatus>,
    /// Index into `login_rows` awaiting a y/n confirmation before logout.
    login_pending_delete: Option<usize>,
    /// When set, the input box is entering an API key for this provider —
    /// same trick as `editing_system_prompt`, but the text renders masked.
    editing_api_key: Option<Provider>,
    /// Providers whose key currently resolves. Tests set this directly and
    /// never call `auth::resolve`.
    connected: Vec<Provider>,
    /// Catalog ids for `connected`, in catalog order.
    available: Vec<&'static str>,
    settings_selected: usize,
    /// Cursor inside the model-picker overlay (independent of `settings_selected`).
    model_selected: usize,
    /// When true, the input box is editing `settings.system_prompt` instead of a chat message.
    editing_system_prompt: bool,
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
    fn new(agent: Agent, settings: Settings, loaded: Option<Session>) -> App {
        let (session, entries, status) = match loaded {
            Some(s) => {
                let status = format!("Resumed '{}'", s.title);
                let entries = entries_from_session(&s);
                (s, entries, status)
            }
            None => (
                Session::new(settings.clone()),
                Vec::new(),
                "Type a message and press Enter · /help for commands".into(),
            ),
        };
        // Tests construct App with dummy() then set `connected` themselves —
        // calling resolve() here would make cargo test depend on $HOME.
        let (connected, available) = if cfg!(test) {
            (Vec::new(), Vec::new())
        } else {
            let c = auth::connected_providers();
            let a = config::available_ids(&c);
            (c, a)
        };
        App {
            login_selected: 0,
            login_rows: Vec::new(),
            login_pending_delete: None,
            editing_api_key: None,
            connected,
            available,
            agent,
            session,
            settings,
            sessions_dir: crate::session::sessions_dir(),
            entries,
            input: String::new(),
            cursor: 0,
            input_scroll: 0,
            focus: Focus::Input,
            status,
            scroll: 0,
            follow: true,
            settings_selected: 0,
            model_selected: 0,
            editing_system_prompt: false,
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
            Focus::Model => self.handle_model_key(key.code),
            Focus::Login => self.handle_login_key(key.code, terminal),
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
        // The command popup owns the arrow keys and Enter; Char/Backspace/
        // Tab/Esc fall through unchanged so typing and the existing bindings
        // keep working while the popup is open.
        if self.command_popup_active() {
            match key.code {
                // While the popup has the line, Enter picks the highlighted
                // command instead of submitting the raw text — it never
                // reaches the chat send path until the popup is closed.
                KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.apply_selected_command(terminal);
                    return;
                }
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
            KeyCode::Esc if self.editing_system_prompt => {
                self.editing_system_prompt = false;
                self.input.clear();
                self.cursor = 0;
                self.input_scroll = 0;
                self.status = "system prompt edit cancelled".into();
            }
            KeyCode::Esc if self.editing_api_key.is_some() => {
                self.editing_api_key = None;
                self.input.clear();
                self.cursor = 0;
                self.input_scroll = 0;
                self.focus = Focus::Login;
                self.status = "key entry cancelled".into();
            }
            KeyCode::Esc => self.quit = true,
            KeyCode::Tab => {
                self.focus = Focus::Settings;
                self.settings_selected = 0;
            }
            KeyCode::Enter if self.editing_api_key.is_some() => {
                self.finish_key_entry(terminal);
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.insert_char('\n');
                self.cmd_selected = 0;
                self.sync_input_scroll(width);
            }
            KeyCode::Enter if self.editing_system_prompt => {
                self.settings.system_prompt = std::mem::take(&mut self.input);
                self.cursor = 0;
                self.input_scroll = 0;
                self.editing_system_prompt = false;
                self.status = format!(
                    "system prompt updated ({} chars)",
                    self.settings.system_prompt.chars().count()
                );
            }
            KeyCode::Enter if !self.input.trim().is_empty() => {
                let line = self.input.trim().to_string();
                // Keyless guard: slash commands still work (that's how you
                // reach /login), but a chat message has nowhere to go.
                if !line.starts_with('/') && !self.agent.has_api_key() {
                    self.status = "no API key — /login to connect one (Enter opens the panel)".into();
                    return;
                }
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
    /// Left, not editing a system prompt or an API key, and at least one
    /// command matches the typed prefix. While this holds, the popup owns
    /// Enter (see `handle_input_key`).
    fn command_popup_active(&self) -> bool {
        !self.editing_system_prompt
            && self.editing_api_key.is_none()
            && !self.cmd_popup_dismissed
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
        let row = SETTINGS_ROWS.get(self.settings_selected).copied().unwrap_or("");
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
            KeyCode::Enter | KeyCode::Char(' ') if row == "model" => self.open_model_picker(),
            KeyCode::Enter | KeyCode::Char(' ') if row == "system_prompt" => {
                self.open_system_prompt_editor()
            }
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
        self.agent.reset();
        *self.agent.settings_mut() = self.settings.clone();
        self.agent.reload_context();
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
        s.context_files = self
            .agent
            .context_files()
            .iter()
            .map(|f| f.path.to_string_lossy().into_owned())
            .collect();
        if let Err(e) = s.save(&self.sessions_dir) {
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
                self.agent.resume(&s);
                self.entries = entries_from_session(&s);
                self.status = format!("Loaded '{}'", s.title);
                self.session = s;
                self.retarget_endpoint();
                self.warn_if_model_unavailable();
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
            "rename" => self.cmd_rename(rest),
            "login" => self.cmd_login(),
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
            "model" => self.cmd_model(rest),
            "system" => self.cmd_system(rest),
            "context" => self.cmd_context(rest),
            "temp" | "temperature" => self.cmd_temp(rest),
            "top-p" | "top_p" => self.cmd_top_p(rest),
            "top-k" | "top_k" => self.cmd_top_k(rest),
            "max-tokens" | "max_tokens" => self.cmd_max_tokens(rest),
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
        // Whatever a command printed landed at the end of the transcript, so
        // keep the view pinned there — /help used to leave the reader in the
        // middle of its own output.
        if self.follow {
            self.scroll_to_bottom(terminal);
        }
    }

    fn cmd_rename(&mut self, rest: &str) {
        match session::rename_session(&self.sessions_dir, &self.session.id, rest) {
            Ok(s) => {
                self.session.title = s.title;
                self.session.updated_at = s.updated_at;
                self.status = format!("renamed to '{}'", self.session.title);
            }
            Err(_) => match self.session.rename(rest) {
                Ok(()) => {
                    self.save_session();
                    self.status = format!("renamed to '{}'", self.session.title);
                }
                Err(e) => self.status = format!("rename failed: {e} (usage: /rename <title>)"),
            },
        }
    }

    fn cmd_model(&mut self, rest: &str) {
        let rest = rest.trim();
        if rest.is_empty() {
            self.open_model_picker();
            return;
        }
        match config::MODEL_CATALOG
            .iter()
            .find(|m| m.id.eq_ignore_ascii_case(rest))
        {
            Some(cm) if self.connected.contains(&cm.provider) => {
                self.apply_model_id(cm);
            }
            Some(cm) => self.entries.push(Entry::Info(format!(
                "no {} key — /login {}",
                cm.provider.label(),
                cm.provider.id()
            ))),
            None => self.entries.push(Entry::Info(config::catalog_error(rest))),
        }
    }

    fn cmd_system(&mut self, rest: &str) {
        match rest.trim() {
            "" => {
                self.entries.push(Entry::Info(format!(
                    "current system prompt:\n{}",
                    self.settings.system_prompt
                )));
                self.status = format!(
                    "system_prompt: {} chars (usage: /system <text> | /system edit | /system clear)",
                    self.settings.system_prompt.chars().count()
                );
            }
            "edit" => self.open_system_prompt_editor(),
            "clear" => {
                self.settings.system_prompt.clear();
                self.status = "system prompt cleared".into();
            }
            text => {
                self.settings.system_prompt = config::unescape(text);
                self.status = format!(
                    "system prompt set ({} chars)",
                    self.settings.system_prompt.chars().count()
                );
            }
        }
    }

    fn cmd_top_p(&mut self, rest: &str) {
        let rest = rest.trim();
        if rest.is_empty() {
            self.status = format!(
                "top_p={} (usage: /top-p off | /top-p {}-{})",
                self.settings
                    .top_p
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "provider default".into()),
                config::TOP_P_MIN,
                config::TOP_P_MAX
            );
            return;
        }
        if rest.eq_ignore_ascii_case("off") {
            self.settings.top_p = None;
            self.status = "top_p off (provider default)".into();
            return;
        }
        match rest
            .parse::<f32>()
            .map_err(|_| format!("not a number: {rest}"))
            .and_then(config::parse_top_p)
        {
            Ok(p) => {
                self.settings.top_p = Some(p);
                self.status = format!("top_p set to {p}");
            }
            Err(e) => self.entries.push(Entry::Info(e)),
        }
    }

    fn cmd_top_k(&mut self, rest: &str) {
        let rest = rest.trim();
        if rest.is_empty() {
            self.status = format!(
                "top_k={} (usage: /top-k off | /top-k full | /top-k <positive int>)",
                self.settings
                    .top_k
                    .map(config::render_top_k)
                    .unwrap_or_else(|| "provider default".into())
            );
            return;
        }
        if rest.eq_ignore_ascii_case("off") {
            self.settings.top_k = None;
            self.status = "top_k off (provider default)".into();
            return;
        }
        if rest.eq_ignore_ascii_case("full") {
            self.settings.top_k = Some(-1);
            self.status = "top_k set to full".into();
            return;
        }
        match rest.parse::<i32>() {
            Ok(k) if k > 0 => {
                self.settings.top_k = Some(k);
                self.status = format!("top_k set to {k}");
            }
            Ok(k) => self.status = format!("top_k must be -1 (off) or a positive count (got {k})"),
            Err(_) => self.status = format!("not a number: {rest}"),
        }
    }

    fn cmd_max_tokens(&mut self, rest: &str) {
        let rest = rest.trim();
        if rest.is_empty() {
            self.status = format!(
                "max_tokens={} (usage: /max-tokens off | /max-tokens {}-{})",
                self.settings
                    .budget_tokens
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "off".into()),
                config::MAX_TOKENS_MIN,
                config::MAX_TOKENS_MAX
            );
            return;
        }
        if rest.eq_ignore_ascii_case("off") {
            self.settings.budget_tokens = None;
            self.status = "max_tokens off".into();
            return;
        }
        match rest
            .parse::<u32>()
            .map_err(|_| format!("not a number: {rest}"))
            .and_then(config::parse_max_tokens)
        {
            Ok(n) => {
                self.settings.budget_tokens = Some(n);
                self.status = format!("max_tokens set to {n}");
            }
            Err(e) => self.entries.push(Entry::Info(e)),
        }
    }

    fn open_model_picker(&mut self) {
        self.model_selected = self
            .available
            .iter()
            .position(|m| *m == self.settings.model)
            .unwrap_or(0);
        self.focus = Focus::Model;
    }

    fn handle_model_key(&mut self, code: KeyCode) {
        let n = self.available.len();
        match code {
            KeyCode::Esc => self.focus = Focus::Settings,
            KeyCode::Up | KeyCode::Char('k') if n > 0 => {
                self.model_selected = (self.model_selected + n - 1) % n;
            }
            KeyCode::Down | KeyCode::Char('j') if n > 0 => {
                self.model_selected = (self.model_selected + 1) % n;
            }
            KeyCode::Enter if n > 0 => self.apply_model_choice(),
            _ => {}
        }
    }

    fn apply_model_choice(&mut self) {
        let Some(id) = self.available.get(self.model_selected).copied() else {
            return;
        };
        let Some(cm) = config::find_model(id) else {
            return;
        };
        self.apply_model_id(cm);
        self.focus = Focus::Settings;
    }

    fn apply_model_id(&mut self, cm: &config::CatalogModel) {
        self.settings.model = cm.id.to_string();
        self.status = if cm.live {
            format!("model set to {}", cm.id)
        } else {
            format!(
                "model set to {} — refused at send (paid); `ask --models` lists live ids",
                cm.id
            )
        };
        self.retarget_endpoint();
    }

    fn retarget_endpoint(&mut self) {
        if cfg!(test) {
            return;
        }
        match Endpoint::for_model(&self.settings.model) {
            Ok(ep) => self.agent.set_endpoint(ep),
            Err(e) => {
                // Keep the old endpoint rather than pointing at a provider
                // that does not match settings.model.
                self.status = e;
            }
        }
    }

    fn warn_if_model_unavailable(&mut self) {
        let Some(p) = config::provider_of(&self.settings.model) else {
            return;
        };
        if self.connected.contains(&p) {
            return;
        }
        self.entries.push(Entry::Info(format!(
            "{} is no longer available — its key is gone (/login {})",
            self.settings.model,
            p.id()
        )));
    }

    #[cfg(test)]
    fn with_connected(&mut self, connected: Vec<Provider>) {
        self.connected = connected;
        self.available = config::available_ids(&self.connected);
    }

    fn open_system_prompt_editor(&mut self) {
        self.input = self.settings.system_prompt.clone();
        self.cursor = self.input.chars().count();
        self.input_scroll = 0;
        self.editing_system_prompt = true;
        self.focus = Focus::Input;
        self.status = "editing system prompt — Enter save · Shift+Enter newline · Esc cancel".into();
    }


    // --- /login: connect personal provider keys -----------------------------

    /// Keyless boot: the agent came up on an unusable endpoint, so open the
    /// login panel right away instead of letting the first send fail.
    fn start_keyless(&mut self, message: &str) {
        self.focus = Focus::Login;
        self.status = message.to_string();
        self.refresh_login_rows();
    }

    fn cmd_login(&mut self) {
        self.login_selected = 0;
        self.login_pending_delete = None;
        self.refresh_login_rows();
        self.focus = Focus::Login;
    }

    fn refresh_login_rows(&mut self) {
        self.login_rows = auth::status_all();
        if self.login_selected >= self.login_rows.len() {
            self.login_selected = 0;
        }
        if !cfg!(test) {
            self.connected = auth::connected_providers();
            self.available = config::available_ids(&self.connected);
        }
    }


    fn handle_login_confirm_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.logout_selected_login(),
            _ => self.login_pending_delete = None,
        }
    }

    fn logout_selected_login(&mut self) {
        let Some(idx) = self.login_pending_delete.take() else { return };
        let Some(row) = self.login_rows.get(idx) else { return };
        let provider = row.provider;
        let model_hit = config::provider_of(&self.settings.model) == Some(provider);
        match auth::disconnect(provider) {
            Ok(true) => {
                if cfg!(test) {
                    self.connected.retain(|p| *p != provider);
                    self.available = config::available_ids(&self.connected);
                }
                if model_hit {
                    self.agent.set_endpoint(Endpoint::unusable());
                    self.status = format!(
                        "{} is no longer available — its key was removed (/login {})",
                        self.settings.model,
                        provider.id()
                    );
                } else {
                    self.status = format!(
                        "{}: key removed from {}",
                        provider.label(),
                        auth::auth_path().display()
                    );
                }
            }
            Ok(false) => {
                self.status = format!(
                    "{}: nothing in the local store — an env var, if any, stays",
                    provider.label()
                );
            }
            Err(e) => self.status = format!("logout failed: {e}"),
        }
        self.refresh_login_rows();
    }

    /// Enter on a provider row: the input box switches to masked key entry
    /// (same mechanism as the system-prompt editor).
    fn begin_key_entry(&mut self) {
        let Some(row) = self.login_rows.get(self.login_selected) else { return };
        let provider = row.provider;
        self.editing_api_key = Some(provider);
        self.input.clear();
        self.cursor = 0;
        self.input_scroll = 0;
        self.focus = Focus::Input;
        self.status = format!(
            "entering {} API key — Enter submits, Esc cancels (checked live, stored 0600)",
            provider.label()
        );
    }

    fn handle_login_key(&mut self, code: KeyCode, terminal: &mut DefaultTerminal) {
        if self.login_pending_delete.is_some() {
            self.handle_login_confirm_key(code);
            return;
        }
        match code {
            KeyCode::Esc | KeyCode::Tab => self.focus = Focus::Input,
            KeyCode::Up | KeyCode::Char('k') => self.move_login_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_login_selection(1),
            KeyCode::Enter => self.begin_key_entry(),
            KeyCode::Char('r') => self.recheck_selected_login(terminal),
            KeyCode::Char('d') | KeyCode::Delete => {
                if self
                    .login_rows
                    .get(self.login_selected)
                    .is_some_and(auth::ProviderStatus::connected)
                {
                    self.login_pending_delete = Some(self.login_selected);
                }
            }
            _ => {}
        }
    }

    fn move_login_selection(&mut self, delta: i32) {
        let n = Provider::ALL.len() as i32;
        self.login_selected =
            ((self.login_selected as i32 + delta).rem_euclid(n)) as usize;
    }

    fn finish_key_entry(&mut self, terminal: &mut DefaultTerminal) {
        let Some(provider) = self.editing_api_key.take() else { return };
        let key = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.input_scroll = 0;
        self.focus = Focus::Login;
        if key.trim().is_empty() {
            self.status = "empty key — nothing connected".into();
            return;
        }
        let label = provider.label();
        let result = self.with_spinner(
            terminal,
            &format!("checking {label} key live"),
            move || auth::connect(provider, &key),
        );
        match result {
            Some(Ok((verdict, saved))) => {
                match &verdict {
                    CheckResult::Confirmed { evidence } => {
                        self.after_login_success(provider);
                        self.status = format!(
                            "{label}: CONFIRMED — key saved to {}",
                            auth::auth_path().display()
                        );
                        self.entries.push(Entry::Info(format!("/login {label}: {evidence}")));
                        self.scroll_to_bottom(terminal);
                    }
                    CheckResult::Unreachable { msg } => {
                        self.after_login_success(provider);
                        self.status = format!(
                            "{label}: provider unreachable ({msg}) — key saved UNVERIFIED, press r to recheck"
                        );
                    }
                    CheckResult::Rejected { http, msg } => {
                        self.status =
                            format!("{label}: REJECTED (HTTP {http}) — key NOT saved ({msg})");
                    }
                }
                if !saved {
                    self.entries.push(Entry::Info(format!(
                        "/login {label}: connect ended without a save ({})",
                        login_verdict_line(&verdict)
                    )));
                }
            }
            Some(Err(e)) => self.status = format!("login failed: {e}"),
            None => self.status = "login cancelled (Esc)".into(),
        }
        self.refresh_login_rows();
    }
    /// A freshly connected key upgrades a keyless boot when it owns the
    /// current model, or when it is the only connected provider.
    fn after_login_success(&mut self, provider: Provider) {
        if cfg!(test) {
            if !self.connected.contains(&provider) {
                self.connected.push(provider);
                self.available = config::available_ids(&self.connected);
            }
        } else {
            self.connected = auth::connected_providers();
            self.available = config::available_ids(&self.connected);
        }
        if self.agent.has_api_key() {
            return;
        }
        let owns = config::provider_of(&self.settings.model) == Some(provider);
        let only = self.connected.len() == 1;
        if owns || only {
            self.retarget_endpoint();
        }
    }

    /// `r` on the panel: live-recheck the selected provider's key.
    fn recheck_selected_login(&mut self, terminal: &mut DefaultTerminal) {
        let Some(row) = self.login_rows.get(self.login_selected) else { return };
        let provider = row.provider;
        let label = provider.label();
        let key = match auth::resolve(provider) {
            Ok(r) => r.key,
            Err(e) => {
                self.status = format!("nothing to check for {label}: {e}");
                return;
            }
        };
        let result = self.with_spinner(
            terminal,
            &format!("rechecking {label} key"),
            move || auth::check(provider, &key),
        );
        match result {
            Some(verdict) => {
                match auth::record_check(provider, &verdict) {
                    Ok(_) => {}
                    Err(e) => self.entries.push(Entry::Info(format!(
                        "could not record {label} check: {e}"
                    ))),
                }
                self.status = format!("{label}: {}", login_verdict_line(&verdict));
                self.refresh_login_rows();
            }
            None => self.status = "recheck cancelled (Esc)".into(),
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
    fn cmd_temp(&mut self, rest: &str) {
        let rest = rest.trim();
        if rest.is_empty() {
            self.status = format!(
                "temperature={} (usage: /temp off | /temp 0.0-{})",
                self.settings
                    .temperature
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "off".into()),
                config::TEMP_MAX
            );
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
    fn prepared_agent(&self) -> Agent {
        let mut agent = self.agent.clone();
        *agent.settings_mut() = self.settings.clone();
        agent.settings_mut().clamp();
        agent.reload_context();
        agent
    }

    fn context_listing(&self) -> String {
        let bundle = self.agent.context();
        let mut lines = vec![format!(
            "context={}  cwd={}",
            if bundle.enabled { "on" } else { "off" },
            bundle.cwd.display()
        )];
        if bundle.files.is_empty() {
            lines.push("no instruction files loaded".into());
        } else {
            for f in self.agent.context_files() {
                lines.push(format!(
                    "{}  {}  {} chars{}",
                    f.scope.as_str(),
                    f.path.display(),
                    f.chars,
                    if f.truncated { "  truncated" } else { "" }
                ));
            }
        }
        lines.join("\n")
    }

    fn cmd_context(&mut self, rest: &str) {
        match rest.trim() {
            "" | "show" => {
                self.entries.push(Entry::Info(self.context_listing()));
            }
            "on" => {
                self.agent.set_context_enabled(true);
                self.settings.context_enabled = true;
                self.status = "context on".into();
                self.entries.push(Entry::Info(self.context_listing()));
            }
            "off" => {
                self.agent.set_context_enabled(false);
                self.settings.context_enabled = false;
                self.status = "context off".into();
            }
            "reload" => {
                let cwd = self.agent.cwd().to_path_buf();
                self.agent.set_cwd(&cwd);
                self.status = "context reloaded".into();
                self.entries.push(Entry::Info(self.context_listing()));
            }
            _ => self.status = "usage: /context [show|on|off|reload]".into(),
        }
    }

    fn json_edit(&mut self, instruction: &str, terminal: &mut DefaultTerminal) {
        let system = format!(
            "You maintain a JSON Schema (draft-like: type/properties/required/additionalProperties). \
             Current schema:\n{}\n\nRewrite it per the user's instruction. \
             Reply with ONLY the raw updated JSON Schema, no prose, no code fences.",
            serde_json::to_string(&self.settings.json_mode.schema).unwrap_or_default()
        );
        let history = vec![ChatMessage::user(instruction)];
        let agent = self.prepared_agent();
        let result = self.with_spinner(terminal, "updating schema", move || {
            agent.complete_with_system(&system, &history)
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

    fn cmd_verify(&mut self, _rest: &str, terminal: &mut DefaultTerminal) {
        let result = self.with_spinner(
            terminal,
            "verifying z.ai levers (live glm-5.3-flash)",
            verify::run,
        );
        match result {
            Some(Ok(report)) => {
                self.status = report.status_line();
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
            let agent = self.prepared_agent();
            let history = vec![ChatMessage::user(question.clone())];
            let label = format!("thinking as {persona} ({}/{n})", i + 1);
            let result = self.with_spinner(terminal, &label, move || {
                agent.complete_with_system(&system, &history)
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
        match SETTINGS_ROWS.get(self.settings_selected).copied().unwrap_or("") {
            "model" => {
                if self.available.is_empty() {
                    return;
                }
                self.settings.model =
                    cycle_choice(&self.available, self.settings.model.as_str(), delta).to_string();
                self.retarget_endpoint();
            }
            "effort" => self.settings.effort = self.settings.effort.cycle(delta),
            "json mode" => self.settings.json_mode.enabled = !self.settings.json_mode.enabled,
            "max_chars" => {
                self.settings.max_chars = cycle_choice(MAX_CHARS_CHOICES, self.settings.max_chars, delta)
            }
            "budget_tokens" => {
                self.settings.budget_tokens =
                    cycle_choice(BUDGET_CHOICES, self.settings.budget_tokens, delta)
            }
            "stop" => {
                let current = STOP_PRESETS
                    .iter()
                    .position(|p| p.iter().map(|s| s.to_string()).collect::<Vec<_>>() == self.settings.stop)
                    .unwrap_or(0) as i32;
                let n = STOP_PRESETS.len() as i32;
                let next = STOP_PRESETS[(current + delta).rem_euclid(n) as usize];
                self.settings.stop = next.iter().map(|s| s.to_string()).collect();
            }
            "temperature" => {
                self.settings.temperature = cycle_float(TEMP_CHOICES, self.settings.temperature, delta)
            }
            "top_p" => self.settings.top_p = cycle_float(TOP_P_CHOICES, self.settings.top_p, delta),
            "top_k" => self.settings.top_k = cycle_choice(TOP_K_CHOICES, self.settings.top_k, delta),
            "system_prompt" => {}
            _ => {}
        }
    }

    fn send_message(&mut self, question: String, terminal: &mut DefaultTerminal) {
        self.session.push_user(question.clone());
        self.entries.push(Entry::User(question));
        self.follow = true;
        self.agent.set_history(self.session.history());
        let history = self.agent.history().to_vec();

        // JSON mode stays on the blocking path: streamed fragments of a JSON
        // object aren't valid JSON until the last token, so there's nothing
        // meaningful to render live, and `render_json_reply` needs the whole
        // body to flatten anyway.
        if self.settings.json_mode.enabled {
            let agent = self.prepared_agent();
            let hist = history.clone();
            let result = self.with_spinner(terminal, "model is thinking", move || {
                agent.complete_outcome(&hist)
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
        let agent = self.prepared_agent();
        let cancel_worker = cancel.clone();
        thread::spawn(move || {
            let mut stream =
                match agent.stream(&history, Some(cancel_worker)) {
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
            self.session.push_assistant_interrupted(capped);
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
            Constraint::Length(1),
            Constraint::Min(4),
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
        } else if self.focus == Focus::Model {
            self.draw_model_overlay(f, body);
        } else if self.focus == Focus::Login {
            self.draw_login_overlay(f, body);
        } else if self.focus == Focus::Input && self.command_popup_active() {
            self.draw_command_popup(f, body);
        }
    }

    /// Input box: renders only the wrapped lines visible inside the (at most
    /// 4-row) window and places the text cursor at the cursor offset. The
    /// same `wrap_input` output drives layout, rendering and cursor math, so
    /// they cannot drift out of sync.
    fn draw_input(&self, f: &mut Frame, area: Rect, width: u16) {
        let focused = self.focus == Focus::Input;
        let border_style = if focused { accent() } else { muted() };
        // API-key entry renders the same char count as '*' so the wrap and
        // cursor math above stay exact while nothing readable is on screen.
        let shown = if self.editing_api_key.is_some() {
            mask_input(&self.input)
        } else {
            self.input.clone()
        };
        let lines = input_lines(&shown, width);
        let view = (area.height.saturating_sub(2)) as usize;
        let scroll = self
            .input_scroll
            .min(lines.len().saturating_sub(view.min(lines.len())));
        // The prompt glyph lives in the first two columns of every row (a
        // blank continuation under it), which is why `input_width` reserves
        // them — wrap, layout and cursor math all measure the same box.
        let mut visible: Vec<Line> = lines
            .iter()
            .enumerate()
            .skip(scroll)
            .take(view)
            .map(|(i, (_, text))| {
                let marker = if i == 0 { "\u{203a} " } else { "  " };
                Line::from(vec![
                    Span::styled(marker, if focused { accent() } else { muted() }),
                    Span::raw(text.clone()),
                ])
            })
            .collect();
        // Placeholder: the hint that used to live in the box title, shown
        // only while there is nothing to read under it.
        if self.input.is_empty() && !self.editing_system_prompt && self.editing_api_key.is_none() {
            visible = vec![Line::from(vec![
                Span::styled("\u{203a} ", if focused { accent() } else { muted() }),
                Span::styled("Ask anything, or / for commands", muted()),
            ])];
        }
        let mut block = Block::bordered().border_type(BorderType::Rounded).border_style(border_style);
        if let Some(provider) = self.editing_api_key {
            block = block
                .title(Span::styled(format!(" {} API key ", provider.label()), accent()))
                .title_bottom(Span::styled(" Enter submit \u{b7} Esc cancel ", muted()));
        } else if self.editing_system_prompt {
            block = block
                .title(Span::styled(" system prompt ", accent()))
                .title_bottom(Span::styled(" Enter save \u{b7} Shift+Enter newline \u{b7} Esc cancel ", muted()));
        }
        f.render_widget(Paragraph::new(visible).block(block), area);
        if focused {
            let (row, col) = cursor_visual_pos(&lines, self.cursor);
            if row >= scroll && row < scroll + view {
                let cx = area.x + 3 + (col as u16).min(area.width.saturating_sub(5));
                let cy = area.y + 1 + (row - scroll) as u16;
                f.set_cursor_position((cx, cy));
            }
        }
    }

    /// The header's right edge: the model, the reasoning effort, the context
    /// state — plus only the levers that are actually doing something.
    /// `Settings::summary` spells out every knob including the ones left at
    /// their default, which is right for `--verify` output and pure noise on
    /// a single header line, where it crowds out the values that matter.
    fn header_meta(&self) -> String {
        let s = &self.settings;
        let bundle = self.agent.context();
        let mut parts = vec![s.model.clone(), format!("effort {}", s.effort.wire())];
        parts.push(if !bundle.enabled {
            "ctx off".into()
        } else {
            format!("ctx {}", bundle.files.len())
        });
        if s.json_mode.enabled {
            parts.push("json".into());
        }
        if let Some(t) = s.temperature {
            parts.push(format!("temp {t}"));
        }
        if let Some(p) = s.top_p {
            parts.push(format!("top_p {p}"));
        }
        if let Some(k) = s.top_k {
            parts.push(format!("top_k {}", config::render_top_k(k)));
        }
        if let Some(n) = s.budget_tokens {
            parts.push(format!("max_tokens {n}"));
        }
        if let Some(n) = s.max_chars {
            parts.push(format!("max_chars {n}"));
        }
        if !s.stop.is_empty() {
            parts.push(format!("stop {}", s.stop.len()));
        }
        parts.join(" \u{b7} ")
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        f.render_widget(Paragraph::new(self.header_line(area.width)), area);
    }

    /// A single unboxed line: the name and the chat title on the left, the
    /// settings the model actually runs with pushed to the right edge in the
    /// muted tone — status, not decoration.
    fn header_line(&self, width: u16) -> Line<'static> {
        let meta = self.header_meta();
        let meta_w = meta.chars().count() + 1;
        // The settings the model actually runs with are the part worth
        // keeping when the window is narrow, so the chat title gives up its
        // columns first (and disappears entirely before the meta clips).
        let room = (width as usize).saturating_sub(4 + 2 + meta_w + 1);
        let full = self.session.title.chars().count();
        let title = if full <= room {
            self.session.title.clone()
        } else if room >= 4 {
            let mut t: String = self.session.title.chars().take(room - 1).collect();
            t.push('\u{2026}');
            t
        } else {
            String::new()
        };
        let left_w = 4 + 2 + title.chars().count();
        let gap = (width as usize).saturating_sub(left_w + meta_w).max(1);
        Line::from(vec![
            Span::styled(" ask", accent().add_modifier(Modifier::BOLD)),
            Span::styled(format!("  {title}"), muted()),
            Span::raw(" ".repeat(gap)),
            Span::styled(format!("{meta} "), muted()),
        ])
    }

    /// The transcript as styled lines. Roles are told apart by a single
    /// glyph in the margin (\u{203a} you, \u{25cf} the model, \u{00b7} the app) rather than by
    /// shouting ALL-CAPS headers, which is what makes the pane read as a
    /// conversation instead of a log.
    fn transcript_lines(&self) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = Vec::new();
        for entry in &self.entries {
            match entry {
                Entry::User(text) => {
                    out.extend(marked_lines("\u{203a} ", muted(), text, Style::default().add_modifier(Modifier::BOLD)));
                    out.push(Line::raw(""));
                }
                Entry::Assistant { text, note } => {
                    out.extend(marked_lines("\u{25cf} ", accent(), text, Style::default()));
                    if let Some(note) = note {
                        out.push(Line::styled(format!("  \u{2937} {note}"), muted()));
                    }
                    out.push(Line::raw(""));
                }
                Entry::Info(text) => {
                    out.extend(marked_lines("\u{00b7} ", muted(), text, muted()));
                    out.push(Line::raw(""));
                }
            }
        }
        if let Some((label, frame)) = &self.spinner {
            out.push(Line::from(vec![
                Span::styled(format!("{frame} "), accent()),
                Span::styled(format!("{label}\u{2026}"), muted()),
            ]));
        } else if out.is_empty() {
            out.push(Line::styled("Ask anything.", muted()));
            out.push(Line::styled("\u{203a} type / for commands, Tab for settings", muted()));
        }
        out
    }

    /// Draw-time clamp: growing the input box shrinks the transcript area, so
    /// a stale offset could exceed `max_scroll` — and `Paragraph::scroll`
    /// clips rather than clamps, which would render a blank pane.
    fn draw_transcript(&self, f: &mut Frame, area: Rect) {
        let max = self.max_scroll_for(f.area().width, f.area().height);
        f.render_widget(
            Paragraph::new(self.transcript_lines())
                .wrap(Wrap { trim: false })
                .scroll((self.scroll.min(max), 0))
                .block(Block::new().padding(Padding::horizontal(1))),
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
    /// the vertical layout in `draw`: header(1) + input + footer(2).
    fn max_scroll_for(&self, width: u16, height: u16) -> u16 {
        let body_height = height.saturating_sub(1 + self.input_box_height(width) + 2);
        let inner_width = width.saturating_sub(2); // horizontal padding
        let total_lines =
            Paragraph::new(self.transcript_lines()).wrap(Wrap { trim: false }).line_count(inner_width) as u16;
        total_lines.saturating_sub(body_height)
    }

    /// Scrolls to the true bottom of the wrapped transcript (see `max_scroll`)
    /// and re-pins the view to incoming content.
    fn scroll_to_bottom(&mut self, terminal: &DefaultTerminal) {
        self.scroll = self.max_scroll(terminal);
        self.follow = true;
    }

    fn draw_settings_overlay(&self, f: &mut Frame, area: Rect) {
        let popup = centered(area, 74, SETTINGS_ROWS.len() as u16 + 2);
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
            List::new(items)
                .highlight_style(selected_row())
                .block(panel("settings", "\u{2191}\u{2193} select \u{b7} \u{2190}\u{2192} change \u{b7} Enter open \u{b7} Esc close")),
            popup,
            &mut state,
        );
    }

    fn setting_value(&self, row: usize) -> String {
        match SETTINGS_ROWS.get(row).copied().unwrap_or("") {
            "model" => {
                let n = self.available.len();
                let total = config::MODEL_CATALOG.len();
                let mut s = format!(
                    "{}  ({n} available of {total} in catalog — Enter opens picker)",
                    self.settings.model
                );
                if config::provider_of(&self.settings.model)
                    .is_some_and(|p| !self.connected.contains(&p))
                {
                    s.push_str("  [no key]");
                }
                s
            }
            "effort" => match config::provider_of(&self.settings.model) {
                Some(p) if p != Provider::Glm => format!(
                    "{}  (glm only — ignored for {})",
                    self.settings.effort,
                    p.id()
                ),
                _ => self.settings.effort.to_string(),
            },
            "json mode" => if self.settings.json_mode.enabled { "on" } else { "off" }.into(),
            "max_chars" => self.settings.max_chars.map(|n| n.to_string()).unwrap_or_else(|| "off".into()),
            "budget_tokens" => {
                self.settings.budget_tokens.map(|n| n.to_string()).unwrap_or_else(|| "off".into())
            }
            "stop" => config::render_stops(&self.settings.stop),
            "temperature" => {
                self.settings.temperature.map(|t| t.to_string()).unwrap_or_else(|| "off".into())
            }
            "top_p" => self
                .settings
                .top_p
                .map(|p| p.to_string())
                .unwrap_or_else(|| "provider default".into()),
            "top_k" => self
                .settings
                .top_k
                .map(config::render_top_k)
                .unwrap_or_else(|| "provider default".into()),
            "system_prompt" => {
                let text = &self.settings.system_prompt;
                let preview: String = text.chars().take(40).collect();
                let suffix = if text.chars().count() > 40 { "…" } else { "" };
                format!("{preview}{suffix}  ({} chars, Enter to edit)", text.chars().count())
            }
            _ => String::new(),
        }
    }

    fn draw_model_overlay(&self, f: &mut Frame, area: Rect) {
        let rows = if self.available.is_empty() {
            1
        } else {
            self.available.len()
        };
        let popup = centered(
            area,
            78,
            (rows as u16 + 2)
                .min(area.height.saturating_sub(2))
                .max(4),
        );
        let items: Vec<ListItem> = if self.available.is_empty() {
            vec![ListItem::new("no providers connected — press Esc, then /login")]
        } else {
            self.available
                .iter()
                .enumerate()
                .map(|(i, id)| {
                    let cursor = if i == self.model_selected { "▸ " } else { "  " };
                    let applied = *id == self.settings.model;
                    let live = config::find_model(id).is_some_and(|m| m.live);
                    let mark = if live { "● " } else { "○ " };
                    let style = if applied {
                        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    ListItem::new(Line::from(vec![
                        Span::raw(cursor),
                        Span::styled(format!("{mark}{id}"), style),
                    ]))
                })
                .collect()
        };
        let mut state = ListState::default();
        if !self.available.is_empty() {
            state.select(Some(self.model_selected.min(self.available.len() - 1)));
        }
        f.render_widget(ratatui::widgets::Clear, popup);
        f.render_stateful_widget(
            List::new(items).highlight_style(selected_row()).block(panel(
                "model",
                "\u{2191}\u{2193} select \u{b7} Enter apply \u{b7} Esc back    \u{25cf} live \u{b7} \u{25cb} refused at send",
            )),
            popup,
            &mut state,
        );
    }

    /// `/login` panel: one row per provider — connection source, masked key,
    /// and the verdict of the last live check.
    fn draw_login_overlay(&self, f: &mut Frame, area: Rect) {
        let popup = centered(
            area,
            88,
            (Provider::ALL.len() as u16 + 2)
                .min(area.height.saturating_sub(2))
                .max(4),
        );
        let items: Vec<ListItem> = self
            .login_rows
            .iter()
            .enumerate()
            .map(|(i, row)| {
                let cursor = if i == self.login_selected { "▸ " } else { "  " };
                ListItem::new(Line::from(vec![
                    Span::raw(cursor),
                    Span::raw(login_row_text(row)),
                ]))
            })
            .collect();
        let mut state = ListState::default();
        state.select(Some(self.login_selected));
        f.render_widget(ratatui::widgets::Clear, popup);
        f.render_stateful_widget(
            List::new(items).highlight_style(selected_row()).block(panel(
                "login",
                "Enter add key \u{b7} r recheck \u{b7} d remove \u{b7} Esc close",
            )),
            popup,
            &mut state,
        );
        if let Some(idx) = self.login_pending_delete {
            if let Some(row) = self.login_rows.get(idx) {
                self.draw_delete_confirm(f, area, row.provider.label());
            }
        }
    }

    fn draw_sessions_overlay(&self, f: &mut Frame, area: Rect) {
        let popup = centered(area, 78, 12.min(area.height.saturating_sub(2)).max(4));
        let items: Vec<ListItem> = if self.sessions_list.is_empty() {
            vec![ListItem::new("(no saved sessions yet)")]
        } else {
            self.sessions_list
                .iter()
                .enumerate()
                .map(|(i, s)| ListItem::new(s.panel_line(i == self.sessions_selected)))
                .collect()
        };
        let mut state = ListState::default();
        state.select(Some(self.sessions_selected));
        f.render_widget(ratatui::widgets::Clear, popup);
        f.render_stateful_widget(
            List::new(items)
                .highlight_style(selected_row())
                .block(panel("sessions", "\u{2191}\u{2193} select \u{b7} Enter open \u{b7} d delete \u{b7} Esc close")),
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
            Paragraph::new(msg).style(Style::default().fg(Color::Red)).block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::Red))
                    .title(Span::styled(" confirm ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))),
            ),
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
            List::new(items)
                .highlight_style(selected_row())
                .block(panel("commands", "\u{2191}\u{2193} select \u{b7} Enter apply \u{b7} \u{2190} close")),
            popup,
            &mut state,
        );
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let keys = if self.spinner.is_some() {
            "Esc stop generation \u{b7} Ctrl-Q quit"
        } else if self.editing_system_prompt || self.editing_api_key.is_some() {
            "Enter save \u{b7} Esc cancel"
        } else {
            "Enter send \u{b7} / commands \u{b7} Tab settings \u{b7} Ctrl-N new \u{b7} Esc quit"
        };
        f.render_widget(
            Paragraph::new(vec![
                Line::styled(format!(" {}", self.status), accent()),
                Line::styled(format!(" {keys}"), muted()),
            ]),
            area,
        );
    }
}

const HELP: &str = "\
/new                      start a new chat session
/rename <title>           rename the current chat session
/sessions                 list and switch between saved sessions
/effort [low|high|max]   get or set reasoning effort
/model [id]               get, set, or open the model picker (flash/free tiers are live)
/system [text|edit|clear] get, set, edit, or clear the system prompt
/json on|off              toggle structured JSON output
/json fields a,b,c        set a flat schema with these string fields
/json schema <json>       set a full JSON Schema
/json edit <instruction>  ask the model to rewrite the schema
/json show                print the active schema
/temp [off|0.0-2.0]       get or set sampling temperature (2.0 is the provider's max)
/top-p [off|0.01-1.0]     get or set nucleus sampling
/top-k [off|full|N]       get or set top-k cutoff
/max-tokens [off|1-131072] get or set the answer token cap
/stop add <seq>           add a stop sequence (max 4)
/stop clear               clear stop sequences
/verify                   prove the agent reaches z.ai and that levers are honoured
/login                    connect glm/deepseek/openrouter keys (live-checked, stored 0600)
/personas <question>      ask physicist/philosopher/mathematician, one call each, in sequence
/personas a,b,c: <question>   same, with your own cast instead of the default three
/context [show|on|off|reload]  AGENTS.md files in the system prompt
/settings                 open the settings panel (Tab does the same)
/quit                     exit
Esc while generating      stop the current generation (partial reply is kept)
Ctrl-Q                    quit, even mid-generation";

/// One transcript entry as styled lines: `marker` in the left margin of the
/// first row, two spaces of hanging indent under it, so the glyph column
/// stays clean no matter how many lines the text has.
fn marked_lines(marker: &'static str, marker_style: Style, text: &str, text_style: Style) -> Vec<Line<'static>> {
    let mut rows: Vec<Line<'static>> = text
        .lines()
        .enumerate()
        .map(|(i, line)| {
            Line::from(vec![
                Span::styled(if i == 0 { marker } else { "  " }, marker_style),
                Span::styled(line.to_string(), text_style),
            ])
        })
        .collect();
    if rows.is_empty() {
        rows.push(Line::from(Span::styled(marker, marker_style)));
    }
    rows
}

fn entries_from_session(s: &Session) -> Vec<Entry> {
    s.messages
        .iter()
        .filter_map(|m| match m.role.as_str() {
            "user" => Some(Entry::User(m.content.clone())),
            "assistant" => Some(Entry::Assistant {
                text: m.content.clone(),
                note: m.interrupted.then(|| "interrupted".to_string()),
            }),
            _ => None,
        })
        .collect()
}

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

/// Every char becomes '*'; the char count (and therefore the wrap/cursor
/// math over the masked string) is unchanged.
fn mask_input(text: &str) -> String {
    text.chars().map(|_| '*').collect()
}

/// One `/login` panel row: provider, source, masked key, last live check.
fn login_row_text(row: &auth::ProviderStatus) -> String {
    // 11 wide: "openrouter" is exactly 10 chars and ran into the next column.
    let id = format!("{:<11}", row.provider.id());
    match (&row.source, &row.masked) {
        (Some(source), Some(key)) => {
            let check = match &row.last_check {
                Some(c) => format!(
                    "  {} {}",
                    c.verdict,
                    session::format_updated(c.at)
                ),
                None => String::new(),
            };
            format!("{id}{:<30} {key}{check}", source.describe())
        }
        _ => format!("{id}not connected — Enter to add a key"),
    }
}

/// Compact verdict for the status line after a recheck.
fn login_verdict_line(verdict: &CheckResult) -> String {
    match verdict {
        CheckResult::Confirmed { evidence } => format!("CONFIRMED — {evidence}"),
        CheckResult::Rejected { http, msg } => format!("REJECTED (HTTP {http}) — {msg}"),
        CheckResult::Unreachable { msg } => format!("UNREACHABLE — {msg}"),
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

/// Inner text width of the input box for a frame of `width` columns: two
/// border columns plus the two-column prompt margin `draw_input` renders.
fn input_width(width: u16) -> usize {
    width.saturating_sub(4).max(1) as usize
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
            // Hard break (newline) or end of input. The spaces buffered since
            // the last word are content here — no word will follow to absorb
            // them — so they have to be rendered. Dropping them used to leave
            // the visual line shorter than the text, which pinned the cursor
            // in place while the user typed trailing spaces.
            _ => {
                if spaces > 0 {
                    let space_start = i - spaces;
                    for k in 0..spaces {
                        if cur_w == width {
                            lines.push((cur_start, std::mem::take(&mut cur)));
                            cur_start = space_start + k;
                            cur_w = 0;
                        }
                        cur.push(' ');
                        cur_w += 1;
                    }
                    spaces = 0;
                }
                if c == Some('\n') {
                    lines.push((cur_start, std::mem::take(&mut cur)));
                    cur_w = 0;
                    cur_start = i + 1;
                }
                i += 1;
            }
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
        let mut app = App::new(Agent::dummy(), config::Settings::default(), None);
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
    fn header_reports_context_off_and_empty() {
        let mut app = App::new(Agent::dummy(), config::Settings::default(), None);
        app.cmd_context("off");
        assert!(app.header_meta().contains("ctx off"));
        app.cmd_context("on");
        assert!(app.header_meta().contains("ctx 0"));
    }

    #[test]
    fn context_command_toggles_injection() {
        let mut app = App::new(Agent::dummy(), config::Settings::default(), None);
        assert!(app.settings.context_enabled);
        app.cmd_context("off");
        assert!(!app.settings.context_enabled);
        assert!(!app.agent.context().enabled);
        app.cmd_context("on");
        assert!(app.settings.context_enabled);
        assert!(app.agent.context().enabled);
        app.cmd_context("reload");
        assert!(app.context_listing().contains("cwd="));
    }

    #[test]
    fn model_row_cycles_the_catalog_both_ways() {
        let mut app = App::new(Agent::dummy(), config::Settings::default(), None);
        app.with_connected(vec![Provider::Glm]);
        let model_row = SETTINGS_ROWS.iter().position(|r| *r == "model").unwrap();
        app.settings_selected = model_row;
        assert_eq!(app.settings.model, config::DEFAULT_MODEL);
        app.adjust_setting(1);
        assert_eq!(app.settings.model, "glm-4.5");
        app.settings.model = config::DEFAULT_MODEL.to_string();
        app.adjust_setting(-1);
        assert_eq!(app.settings.model, "glm-5.3");
    }

    #[test]
    fn cmd_model_sets_catalog_id_and_rejects_unknown() {
        let mut app = App::new(Agent::dummy(), config::Settings::default(), None);
        app.with_connected(vec![Provider::Glm]);
        app.cmd_model("glm-4.6");
        assert_eq!(app.settings.model, "glm-4.6");
        app.cmd_model("nope");
        assert_eq!(app.settings.model, "glm-4.6");
        let Some(Entry::Info(info)) = app.entries.last() else {
            panic!("expected Info entry after unknown model");
        };
        assert!(info.contains("unknown model `nope`"));
        assert!(info.contains("glm-5.3-flash"));
    }

    #[test]
    fn open_model_picker_seeds_cursor_at_current_model() {
        let mut app = App::new(Agent::dummy(), config::Settings::default(), None);
        app.with_connected(vec![Provider::Glm]);
        app.settings.model = "glm-4.6".into();
        app.open_model_picker();
        assert!(matches!(app.focus, Focus::Model));
        assert_eq!(
            app.model_selected,
            app.available.iter().position(|m| *m == "glm-4.6").unwrap()
        );
    }

    #[test]
    fn model_picker_with_no_keys_is_empty_and_does_not_panic() {
        let mut app = App::new(Agent::dummy(), config::Settings::default(), None);
        assert!(app.available.is_empty());
        app.open_model_picker();
        app.handle_model_key(KeyCode::Down);
        app.handle_model_key(KeyCode::Up);
        app.handle_model_key(KeyCode::Enter);
        assert_eq!(app.settings.model, config::DEFAULT_MODEL);
        assert!(matches!(app.focus, Focus::Model));
    }

    #[test]
    fn model_picker_hides_providers_without_keys() {
        let mut app = App::new(Agent::dummy(), config::Settings::default(), None);
        app.with_connected(vec![Provider::Glm]);
        assert!(app.available.contains(&"glm-5.3-flash"));
        assert!(!app.available.iter().any(|id| id.starts_with("deepseek") || id.contains(":free")));
        app.with_connected(vec![Provider::Glm, Provider::OpenRouter]);
        assert!(app.available.iter().any(|id| id.ends_with(":free")));
        assert!(!app.available.contains(&"deepseek-flash"));
    }

    #[test]
    fn cmd_model_on_unconnected_provider_explains_login() {
        let mut app = App::new(Agent::dummy(), config::Settings::default(), None);
        app.with_connected(vec![Provider::Glm]);
        app.cmd_model("deepseek-flash");
        assert_eq!(app.settings.model, config::DEFAULT_MODEL);
        let Some(Entry::Info(info)) = app.entries.last() else {
            panic!("expected Info entry after unconnected model");
        };
        assert!(info.contains("no DeepSeek key"), "{info}");
        assert!(info.contains("/login deepseek"), "{info}");
        assert!(!info.contains("unknown model"), "{info}");
    }

    #[test]
    fn cmd_system_edit_show_and_set() {
        let mut app = App::new(Agent::dummy(), config::Settings::default(), None);
        let original = app.settings.system_prompt.clone();
        app.cmd_system("");
        assert_eq!(app.settings.system_prompt, original);
        app.cmd_system("edit");
        assert!(app.editing_system_prompt);
        assert_eq!(app.input, original);
        assert!(matches!(app.focus, Focus::Input));
        app.cmd_system("new text");
        assert_eq!(app.settings.system_prompt, "new text");
    }

    #[test]
    fn cmd_top_p_empty_off_set_and_reject() {
        let mut app = App::new(Agent::dummy(), config::Settings::default(), None);
        app.cmd_top_p("");
        assert!(app.settings.top_p.is_none());
        assert!(app.status.contains("usage: /top-p"));
        app.cmd_top_p("0.8");
        assert_eq!(app.settings.top_p, Some(0.8));
        app.cmd_top_p("1.5");
        assert_eq!(app.settings.top_p, Some(0.8));
        app.cmd_top_p("off");
        assert!(app.settings.top_p.is_none());
    }

    #[test]
    fn cmd_top_k_empty_off_full_set_and_reject() {
        let mut app = App::new(Agent::dummy(), config::Settings::default(), None);
        app.cmd_top_k("");
        assert!(app.settings.top_k.is_none());
        assert!(app.status.contains("usage: /top-k"));
        app.cmd_top_k("full");
        assert_eq!(app.settings.top_k, Some(-1));
        app.cmd_top_k("40");
        assert_eq!(app.settings.top_k, Some(40));
        app.cmd_top_k("0");
        assert_eq!(app.settings.top_k, Some(40));
        app.cmd_top_k("nope");
        assert_eq!(app.settings.top_k, Some(40));
        app.cmd_top_k("off");
        assert!(app.settings.top_k.is_none());
    }

    #[test]
    fn cmd_max_tokens_empty_off_set_and_reject() {
        let mut app = App::new(Agent::dummy(), config::Settings::default(), None);
        app.cmd_max_tokens("");
        assert!(app.settings.budget_tokens.is_none());
        assert!(app.status.contains("usage: /max-tokens"));
        app.cmd_max_tokens("256");
        assert_eq!(app.settings.budget_tokens, Some(256));
        app.cmd_max_tokens("0");
        assert_eq!(app.settings.budget_tokens, Some(256));
        app.cmd_max_tokens("off");
        assert!(app.settings.budget_tokens.is_none());
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
    fn wrap_keeps_trailing_spaces_so_the_cursor_can_move_past_them() {
        // Typing "hi " has to leave the cursor after the space: the visual
        // line must be as long as the text, not trimmed back to the last
        // word, or the cursor stalls while the user keeps typing spaces.
        let lines = wrap_input("hi ", 10);
        assert_eq!(lines, vec![(0, "hi ".to_string())]);
        assert_eq!(cursor_visual_pos(&lines, 3), (0, 3));
        let lines = wrap_input("hi   ", 10);
        assert_eq!(cursor_visual_pos(&lines, 5), (0, 5));
        // A line of nothing but spaces still advances the cursor.
        let lines = wrap_input("  ", 10);
        assert_eq!(lines, vec![(0, "  ".to_string())]);
        assert_eq!(cursor_visual_pos(&lines, 2), (0, 2));
        // Spaces before an explicit newline are content too.
        let lines = wrap_input("hi \nx", 10);
        assert_eq!(lines, vec![(0, "hi ".to_string()), (4, "x".to_string())]);
        assert_eq!(cursor_visual_pos(&lines, 3), (0, 3));
    }

    #[test]
    fn trailing_spaces_wrap_onto_the_next_row_at_the_edge() {
        // "aaa bbb" fills the row, so the space after it opens the next one —
        // which is where the word typed after it lands anyway, so the cursor
        // does not jump when the word arrives.
        let lines = wrap_input("aaa bbb ", 7);
        assert_eq!(lines, vec![(0, "aaa bbb".to_string()), (7, " ".to_string())]);
        assert_eq!(cursor_visual_pos(&lines, 8), (1, 1));
        assert_eq!(cursor_visual_pos(&wrap_input("aaa bbb c", 7), 9), (1, 1));
    }

    #[test]
    fn header_gives_up_the_title_before_it_clips_the_settings() {
        let mut app = App::new(Agent::dummy(), config::Settings::default(), None);
        app.session.title = "a very long chat title indeed".into();
        let meta = app.header_meta();
        // Wide: title in full, settings flush to the right edge.
        let wide = app.header_line(120).to_string();
        assert!(wide.starts_with(" ask  a very long chat title indeed"));
        assert!(wide.trim_end().ends_with(&meta));
        assert_eq!(wide.chars().count(), 120);
        // Tight: the title is cut short (with an ellipsis) but the settings
        // still fit whole.
        let tight = app.header_line((meta.chars().count() + 16) as u16).to_string();
        assert!(tight.contains('\u{2026}'));
        assert!(tight.trim_end().ends_with(&meta));
        // Narrower than the settings themselves: the title is gone entirely.
        let narrow = app.header_line((meta.chars().count() + 4) as u16).to_string();
        assert!(!narrow.contains("very long"));
        assert!(narrow.trim_end().ends_with(&meta));
    }

    #[test]
    fn header_meta_names_the_model_and_hides_levers_left_at_default() {
        let mut app = App::new(Agent::dummy(), config::Settings::default(), None);
        let meta = app.header_meta();
        assert!(meta.starts_with(&app.settings.model));
        assert!(!meta.contains("json"), "{meta}");
        assert!(!meta.contains("temp"), "{meta}");
        // A lever that is actually set earns its place on the line.
        app.settings.temperature = Some(0.7);
        app.settings.json_mode.enabled = true;
        let meta = app.header_meta();
        assert!(meta.contains("temp 0.7"), "{meta}");
        assert!(meta.contains("json"), "{meta}");
    }

    #[test]
    fn transcript_marks_each_voice_with_its_own_glyph() {
        let mut app = App::new(Agent::dummy(), config::Settings::default(), None);
        app.entries = vec![
            Entry::User("hi\nthere".into()),
            Entry::Assistant { text: "yo".into(), note: Some("stopped".into()) },
            Entry::Info("note".into()),
        ];
        let rendered: Vec<String> =
            app.transcript_lines().iter().map(|l| l.to_string()).collect();
        // First row of a message carries the glyph, continuations are indented
        // under it, and every entry is followed by a blank separator row.
        assert_eq!(
            rendered,
            vec!["\u{203a} hi", "  there", "", "\u{25cf} yo", "  \u{2937} stopped", "", "\u{b7} note", ""]
        );
    }

    #[test]
    fn empty_transcript_shows_the_placeholder_until_the_spinner_takes_over() {
        let mut app = App::new(Agent::dummy(), config::Settings::default(), None);
        assert!(app.transcript_lines()[0].to_string().starts_with("Ask anything"));
        app.spinner = Some(("thinking".into(), SPINNER_FRAMES[0]));
        let rendered: Vec<String> =
            app.transcript_lines().iter().map(|l| l.to_string()).collect();
        assert_eq!(rendered.len(), 1);
        assert!(rendered[0].contains("thinking"));
    }

    #[test]
    fn command_popup_stays_out_of_api_key_entry() {
        // The popup owns Enter while it is open, so it must not open over an
        // API key that happens to start with a slash.
        let mut app = App::new(Agent::dummy(), config::Settings::default(), None);
        app.input = "/mod".into();
        assert!(app.command_popup_active());
        assert_eq!(app.filtered_commands(), vec!["model"]);
        app.editing_api_key = Some(Provider::ALL[0]);
        assert!(!app.command_popup_active());
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
    fn login_panel_opens_and_rows_render() {
        let mut app = App::new(Agent::dummy(), config::Settings::default(), None);
        app.cmd_login();
        assert!(matches!(app.focus, Focus::Login));
        assert_eq!(app.login_rows.len(), Provider::ALL.len());
        // Selection wraps over exactly the three providers.
        app.move_login_selection(1);
        assert_eq!(app.login_selected, 1);
        app.move_login_selection(-1);
        app.move_login_selection(-1);
        assert_eq!(app.login_selected, Provider::ALL.len() - 1);
        // Row text for an unconnected provider offers the Enter hint.
        let unconnected = auth::ProviderStatus {
            provider: Provider::DeepSeek,
            source: None,
            masked: None,
            last_check: None,
        };
        assert!(login_row_text(&unconnected).contains("not connected"));
        let connected = auth::ProviderStatus {
            provider: Provider::OpenRouter,
            source: Some(auth::Source::Env),
            masked: Some(auth::mask("sk-or-v1-abcdefgh")),
            last_check: None,
        };
        let text = login_row_text(&connected);
        assert!(text.contains("env"));
        assert!(text.contains("sk-or…efgh"));
    }

    #[test]
    fn login_key_entry_focuses_input_and_masks_echo() {
        let mut app = App::new(Agent::dummy(), config::Settings::default(), None);
        app.cmd_login();
        app.begin_key_entry();
        assert!(app.editing_api_key.is_some());
        assert!(matches!(app.focus, Focus::Input));
        for c in "super-secret-key".chars() {
            app.insert_char(c);
        }
        assert_eq!(mask_input(&app.input), "*".repeat("super-secret-key".len()));
        // The masked text must wrap into the same number of visual lines.
        let plain = input_lines(&app.input, 80);
        let masked = input_lines(&mask_input(&app.input), 80);
        assert_eq!(plain.len(), masked.len());
    }

    #[test]
    fn login_logout_needs_explicit_confirmation() {
        let mut app = App::new(Agent::dummy(), config::Settings::default(), None);
        app.cmd_login();
        app.login_pending_delete = Some(0);
        // Any key other than y/Enter aborts the logout — nothing is removed.
        app.handle_login_confirm_key(KeyCode::Char('n'));
        assert!(app.login_pending_delete.is_none());
    }

    #[test]
    fn keyless_boot_lands_on_the_login_panel() {
        let mut app = App::new(Agent::dummy(), config::Settings::default(), None);
        app.start_keyless("no API key yet — connect one below");
        assert!(matches!(app.focus, Focus::Login));
        assert!(!app.login_rows.is_empty());
        assert!(app.status.contains("no API key"));
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
