//! ChatGPT-style console chat, built on ratatui: a scrollback transcript, an
//! input line, and slash commands for sessions, effort, JSON mode, length
//! and stop-condition settings.

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use serde_json::Value;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

use crate::api::{self, ChatMessage, Endpoint};
use crate::config::{self, Effort, Res, Settings};
use crate::render::{self, strip_fences};
use crate::session::{self, Session, SessionSummary};
use crate::verify;

const MAX_CHARS_CHOICES: &[Option<usize>] =
    &[None, Some(100), Some(250), Some(500), Some(1000), Some(2000)];
const BUDGET_CHOICES: &[Option<u32>] =
    &[None, Some(32), Some(64), Some(128), Some(256), Some(512), Some(1024), Some(4096)];
const STOP_PRESETS: &[&[&str]] = &[&[], &["\n\n"], &["\n---\n"], &["\n\n\n"]];
const TEMP_CHOICES: &[Option<f32>] = &[None, Some(0.0), Some(0.3), Some(0.7), Some(1.0)];
const SETTINGS_ROWS: &[&str] = &["effort", "json mode", "max_chars", "budget_tokens", "stop", "temperature"];

pub fn run(settings: Settings) -> Res<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("interactive chat requires a terminal".into());
    }
    let ep = Endpoint::resolve()?;
    let mut terminal = ratatui::try_init().map_err(|e| {
        let _ = ratatui::try_restore();
        format!("cannot start TUI: {e}")
    })?;
    let result = App::new(ep, settings).event_loop(&mut terminal);
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
    focus: Focus,
    status: String,
    scroll: u16,
    settings_selected: usize,
    sessions_list: Vec<SessionSummary>,
    sessions_selected: usize,
    quit: bool,
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
            focus: Focus::Input,
            status: "Type a message and press Enter · /help for commands".into(),
            scroll: 0,
            settings_selected: 0,
            sessions_list: Vec::new(),
            sessions_selected: 0,
            quit: false,
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
            let Event::Key(key) = event::read().map_err(|e| e.to_string())? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }

            if key.modifiers.contains(KeyModifiers::CONTROL) {
                match key.code {
                    KeyCode::Char('n') => self.new_chat(),
                    KeyCode::Char('q') => self.quit = true,
                    _ => {}
                }
                continue;
            }

            match self.focus {
                Focus::Input => self.handle_input_key(key.code, terminal),
                Focus::Settings => self.handle_settings_key(key.code),
                Focus::Sessions => self.handle_sessions_key(key.code, terminal),
            }
        }
        Ok(())
    }

    fn handle_input_key(&mut self, code: KeyCode, terminal: &mut DefaultTerminal) {
        match code {
            KeyCode::Esc => self.quit = true,
            KeyCode::Tab => {
                self.focus = Focus::Settings;
                self.settings_selected = 0;
            }
            KeyCode::Enter if !self.input.trim().is_empty() => {
                let line = std::mem::take(&mut self.input);
                if let Some(cmd) = line.trim().strip_prefix('/') {
                    self.handle_command(cmd.trim(), terminal);
                } else {
                    self.send_message(line.trim().to_string(), terminal);
                }
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(c) => self.input.push(c),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(10),
            KeyCode::Home => self.scroll = 0,
            _ => {}
        }
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
            _ => {}
        }
    }

    fn new_chat(&mut self) {
        self.save_session();
        self.session = Session::new(self.settings.clone());
        self.entries.clear();
        self.scroll = 0;
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
            "json" => self.cmd_json(rest, terminal),
            "stop" => self.cmd_stop(rest),
            "verify" => self.cmd_verify(rest, terminal),
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
        let _ = terminal.draw(|f| draw_busy(f, "updating schema…"));
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
        match api::chat(&self.ep, &plain, &system, &history, None) {
            Ok(outcome) => {
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
            Err(e) => self.status = format!("schema edit failed: {e}"),
        }
    }

    fn cmd_verify(&mut self, rest: &str, terminal: &mut DefaultTerminal) {
        let prompt = if rest.is_empty() {
            verify::DEFAULT_PROMPT.to_string()
        } else {
            rest.to_string()
        };
        let _ = terminal.draw(|f| draw_busy(f, "verifying stop condition (2 calls)…"));
        match verify::run(&self.ep, &self.settings, &prompt) {
            Ok(report) => {
                self.status = if report.stop_condition_had_effect() {
                    "verify: stop condition CONFIRMED working".into()
                } else {
                    "verify: stop condition had NO effect — check budget_tokens/stop".into()
                };
                self.entries.push(Entry::Info(report.render()));
                self.scroll_to_bottom(terminal);
            }
            Err(e) => self.status = format!("verify failed: {e}"),
        }
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
            5 => self.settings.temperature = cycle_temperature(self.settings.temperature, delta),
            _ => {}
        }
    }

    fn send_message(&mut self, question: String, terminal: &mut DefaultTerminal) {
        self.session.push_user(question.clone());
        self.entries.push(Entry::User(question));
        let history = self.session.history();
        let schema = self
            .settings
            .json_mode
            .enabled
            .then(|| self.settings.json_mode.schema.clone());

        let _ = terminal.draw(|f| draw_busy(f, "model is thinking…"));
        match api::chat(&self.ep, &self.settings, "", &history, schema.as_ref()) {
            Ok(outcome) => {
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
                self.entries.push(Entry::Assistant {
                    text: capped,
                    note: (!note_parts.is_empty()).then(|| note_parts.join(" · ")),
                });
                self.status = format!(
                    "finish={} · tokens: prompt={} completion={} (reasoning={}) · {}ms",
                    outcome.finish_reason.as_deref().unwrap_or("?"),
                    outcome.usage.prompt_tokens,
                    outcome.usage.completion_tokens,
                    outcome.usage.reasoning_tokens,
                    outcome.latency_ms
                );
                self.save_session();
                self.scroll_to_bottom(terminal);
            }
            Err(e) => {
                self.session.messages.pop();
                self.status = format!("request failed: {e}");
            }
        }
    }

    fn draw(&self, f: &mut Frame) {
        let [header, body, input, footer] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(3),
            Constraint::Length(2),
        ])
        .areas(f.area());
        self.draw_header(f, header);
        self.draw_transcript(f, body);
        let input_style = if self.focus == Focus::Input {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        };
        f.render_widget(
            Paragraph::new(self.input.as_str())
                .style(input_style)
                .block(Block::bordered().title(" message — /help for commands ")),
            input,
        );
        self.draw_footer(f, footer);

        if self.focus == Focus::Settings {
            self.draw_settings_overlay(f, body);
        } else if self.focus == Focus::Sessions {
            self.draw_sessions_overlay(f, body);
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
        body
    }

    fn draw_transcript(&self, f: &mut Frame, area: Rect) {
        f.render_widget(
            Paragraph::new(self.transcript_text())
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0))
                .block(Block::bordered().title(" conversation ")),
            area,
        );
    }

    /// Scrolls to the true bottom of the wrapped transcript. `scroll:
    /// u16::MAX` looks tempting but `Paragraph::scroll` clips rather than
    /// clamps, so an offset past the content renders a blank pane — this
    /// computes the real last-page offset via `line_count` instead.
    fn scroll_to_bottom(&mut self, terminal: &DefaultTerminal) {
        let Ok(size) = terminal.size() else { return };
        // Mirrors the vertical layout in `draw`: header(3) + input(3) + footer(2).
        let body_height = size.height.saturating_sub(3 + 3 + 2);
        let inner_width = size.width.saturating_sub(2); // block borders
        let inner_height = body_height.saturating_sub(2); // block borders
        let total_lines =
            Paragraph::new(self.transcript_text()).wrap(Wrap { trim: false }).line_count(inner_width) as u16;
        self.scroll = total_lines.saturating_sub(inner_height);
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
            List::new(items).block(Block::bordered().title(" sessions — ↑↓ select, Enter open, Esc close ")),
            popup,
            &mut state,
        );
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let keys = "Enter send/run · Tab settings · Ctrl-N new · /sessions /effort /json /verify /help · Esc quit";
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
/stop add <seq>           add a stop sequence (max 4)
/stop clear               clear stop sequences
/verify [prompt]          prove the stop condition changes the output
/settings                 open the settings panel (Tab does the same)
/quit                     exit";

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

fn draw_busy(f: &mut Frame, msg: &str) {
    let area = f.area();
    f.render_widget(
        Paragraph::new(msg).block(Block::bordered().title(" working ")),
        Rect {
            x: area.x,
            y: area.y + area.height / 2,
            width: area.width,
            height: 3.min(area.height),
        },
    );
}

fn cycle_choice<T: PartialEq + Copy>(choices: &[T], current: T, delta: i32) -> T {
    let i = choices.iter().position(|c| *c == current).unwrap_or(0) as i32;
    let n = choices.len() as i32;
    choices[(i + delta).rem_euclid(n) as usize]
}

/// `f32` doesn't implement `Eq`, so temperature gets its own small cycler
/// over the fixed `TEMP_CHOICES` set (bitwise comparison is fine — these are
/// exact constants, not computed values).
fn cycle_temperature(current: Option<f32>, delta: i32) -> Option<f32> {
    let i = TEMP_CHOICES
        .iter()
        .position(|c| c.map(f32::to_bits) == current.map(f32::to_bits))
        .unwrap_or(0) as i32;
    let n = TEMP_CHOICES.len() as i32;
    TEMP_CHOICES[(i + delta).rem_euclid(n) as usize]
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
    fn cycle_choice_wraps_both_directions() {
        let choices = [None, Some(1u32), Some(2)];
        assert_eq!(cycle_choice(&choices, None, 1), Some(1));
        assert_eq!(cycle_choice(&choices, None, -1), Some(2));
    }
}
