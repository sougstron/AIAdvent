//! Chat-style TUI over the shared generation engine.

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::io::IsTerminal;
use std::time::Duration;

use crate::cli::display_stops;
use crate::compare;
use crate::config::{Res, RunConfig};
use crate::engine::{Engine, RunResult};

const STOP_CHOICES: &[&[&str]] = &[&[], &["\n---\n"], &["\n\n"], &["\n\n\n"]];
const TOKEN_CHOICES: &[Option<u32>] = &[
    None,
    Some(8 * 1024),
    Some(16 * 1024),
    Some(33 * 1024),
    Some(64 * 1024),
];
const ROWS: &[&str] = &[
    "format",
    "thinking",
    "max_tokens",
    "max_items",
    "stop",
    "source",
    "since (h)",
    "limit",
];

pub fn run(cfg: RunConfig) -> Res<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("--tui requires an interactive terminal".into());
    }
    let mut terminal = ratatui::try_init().map_err(|e| {
        let _ = ratatui::try_restore();
        format!("cannot start TUI: {e}")
    })?;
    let result = (|| {
        let _ = terminal.draw(|f| draw_busy(f, "fetching news…"));
        let engine = Engine::new(&cfg)?;
        App::new(cfg, engine).event_loop(&mut terminal)
    })();
    ratatui::restore();
    result
}

struct Turn {
    question: String,
    result: RunResult,
}

struct Chat {
    title: String,
    turns: Vec<Turn>,
}

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Input,
    Settings,
}

struct App {
    cfg: RunConfig,
    engine: Engine,
    chats: Vec<Chat>,
    chat_idx: usize,
    input: String,
    focus: Focus,
    selected: usize,
    stop_idx: usize,
    report: Option<String>,
    status: String,
    scroll: u16,
    show_raw: bool,
    news_dirty: bool,
    quit: bool,
}

impl App {
    fn new(cfg: RunConfig, engine: Engine) -> App {
        let stop_idx = stop_index(&cfg);
        App {
            cfg,
            engine,
            chats: vec![Chat {
                title: "New chat".into(),
                turns: Vec::new(),
            }],
            chat_idx: 0,
            input: String::new(),
            focus: Focus::Input,
            selected: 0,
            stop_idx,
            report: None,
            status: "Type a request and press Enter".into(),
            scroll: 0,
            show_raw: false,
            news_dirty: false,
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
                    KeyCode::Left => self.switch_chat(-1),
                    KeyCode::Right => self.switch_chat(1),
                    KeyCode::Char('q') => self.quit = true,
                    _ => {}
                }
                continue;
            }
            match key.code {
                KeyCode::Esc => self.quit = true,
                KeyCode::Tab => {
                    self.focus = if self.focus == Focus::Input {
                        Focus::Settings
                    } else {
                        Focus::Input
                    }
                }
                KeyCode::Enter if !self.input.trim().is_empty() => self.generate(terminal),
                KeyCode::Backspace if self.focus == Focus::Input => {
                    self.input.pop();
                }
                KeyCode::Char(c) if self.focus == Focus::Input => self.input.push(c),
                KeyCode::Up | KeyCode::Char('k') if self.focus == Focus::Settings => {
                    self.selected = (self.selected + ROWS.len() - 1) % ROWS.len()
                }
                KeyCode::Down | KeyCode::Char('j') if self.focus == Focus::Settings => {
                    self.selected = (self.selected + 1) % ROWS.len()
                }
                KeyCode::Left | KeyCode::Char('h') if self.focus == Focus::Settings => {
                    self.adjust(-1)
                }
                KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ')
                    if self.focus == Focus::Settings =>
                {
                    self.adjust(1)
                }
                KeyCode::Char('r') if self.focus == Focus::Settings => {
                    self.show_raw = !self.show_raw;
                    self.scroll = 0;
                }
                KeyCode::Char('f') if self.focus == Focus::Settings => self.refetch(terminal),
                KeyCode::Char('c') if self.focus == Focus::Settings => self.run_compare(terminal),
                KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
                KeyCode::PageDown => self.scroll = self.scroll.saturating_add(10),
                KeyCode::Home => self.scroll = 0,
                _ => {}
            }
        }
        Ok(())
    }

    fn new_chat(&mut self) {
        self.chats.push(Chat {
            title: format!("Chat {}", self.chats.len() + 1),
            turns: Vec::new(),
        });
        self.chat_idx = self.chats.len() - 1;
        self.input.clear();
        self.report = None;
        self.scroll = 0;
        self.focus = Focus::Input;
        self.status = "New chat created".into();
    }

    fn switch_chat(&mut self, delta: i32) {
        self.chat_idx = (self.chat_idx as i32 + delta).rem_euclid(self.chats.len() as i32) as usize;
        self.report = None;
        self.scroll = 0;
        self.status = format!("Switched to {}", self.chats[self.chat_idx].title);
    }

    fn value(&self, row: usize) -> String {
        match row {
            0 => self.cfg.format.to_string(),
            1 => if self.cfg.thinking { "on" } else { "off" }.into(),
            2 => self
                .cfg
                .max_tokens
                .map(format_tokens)
                .unwrap_or_else(|| "off".into()),
            3 => self
                .cfg
                .max_items
                .map(|n| n.to_string())
                .unwrap_or_else(|| "off".into()),
            4 => display_stops(&self.cfg.stop),
            5 => self.cfg.source.label().to_string(),
            6 => self.cfg.since_hours.to_string(),
            7 => self.cfg.limit.to_string(),
            _ => String::new(),
        }
    }

    fn adjust(&mut self, delta: i32) {
        match self.selected {
            0 => self.cfg.format = self.cfg.format.cycle(delta),
            1 => self.cfg.thinking = !self.cfg.thinking,
            2 => {
                let current = TOKEN_CHOICES
                    .iter()
                    .position(|v| *v == self.cfg.max_tokens)
                    .unwrap_or(0) as i32;
                self.cfg.max_tokens = TOKEN_CHOICES
                    [(current + delta).rem_euclid(TOKEN_CHOICES.len() as i32) as usize];
            }
            3 => {
                self.cfg.max_items =
                    step_opt(self.cfg.max_items.map(|n| n as u32), delta, 1, 30).map(|n| n as usize)
            }
            4 => {
                self.stop_idx =
                    (self.stop_idx as i32 + delta).rem_euclid(STOP_CHOICES.len() as i32) as usize;
                self.cfg.stop = STOP_CHOICES[self.stop_idx]
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
            }
            5 => {
                self.cfg.source = self.cfg.source.cycle(delta);
                self.news_dirty = true;
            }
            6 => {
                self.cfg.since_hours =
                    (self.cfg.since_hours as i64 + delta as i64 * 12).clamp(6, 24 * 60) as u64;
                self.news_dirty = true;
            }
            7 => {
                self.cfg.limit = (self.cfg.limit as i32 + delta).clamp(1, 60) as usize;
                self.news_dirty = true;
            }
            _ => {}
        }
    }

    fn refetch(&mut self, terminal: &mut DefaultTerminal) {
        let _ = terminal.draw(|f| draw_busy(f, "fetching news…"));
        match Engine::new(&self.cfg) {
            Ok(e) => {
                self.status = format!("refetched: {}", e.news_note);
                self.engine = e;
                self.news_dirty = false;
            }
            Err(e) => self.status = format!("fetch failed: {e}"),
        }
    }

    fn generate(&mut self, terminal: &mut DefaultTerminal) {
        let question = self.input.trim().to_string();
        self.cfg.question = question.clone();
        let _ = terminal.draw(|f| draw_busy(f, "searching news for your query…"));
        match Engine::new(&self.cfg) {
            Ok(engine) => {
                self.engine = engine;
                self.news_dirty = false;
            }
            Err(e) => {
                self.status = format!("search failed: {e}");
                return;
            }
        }
        let _ = terminal.draw(|f| draw_busy(f, "model is thinking…"));
        match self.engine.run_once(&self.cfg) {
            Ok(result) => {
                self.status = summarize(&result);
                let chat = &mut self.chats[self.chat_idx];
                if chat.turns.is_empty() {
                    chat.title = question.chars().take(24).collect();
                }
                chat.turns.push(Turn { question, result });
                self.input.clear();
                self.report = None;
                self.scroll = u16::MAX;
            }
            Err(e) => self.status = format!("request failed: {e}"),
        }
    }

    fn run_compare(&mut self, terminal: &mut DefaultTerminal) {
        if self.news_dirty {
            self.refetch(terminal);
        }
        let presets = vec!["baseline".to_string(), "strict".to_string()];
        let out_dir = compare::default_out_dir();
        let mut cfg = self.cfg.clone();
        cfg.runs = 1;
        let result = compare::compare(
            &self.engine,
            &cfg,
            &presets,
            Some(&out_dir),
            |name, i, n| {
                let _ =
                    terminal.draw(|f| draw_busy(f, &format!("comparing `{name}` — run {i}/{n}")));
            },
        );
        match result {
            Ok(reports) => {
                let text = compare::render_report(&self.engine, &cfg, &reports);
                let _ = std::fs::write(out_dir.join("report.md"), &text);
                self.status = format!("2-bot comparison saved to {}", out_dir.display());
                self.report = Some(text);
                self.scroll = 0;
            }
            Err(e) => self.status = format!("comparison failed: {e}"),
        }
    }

    fn last_result(&self) -> Option<&RunResult> {
        self.chats
            .get(self.chat_idx)?
            .turns
            .last()
            .map(|t| &t.result)
    }

    fn draw(&self, f: &mut Frame) {
        let [header, body, input, footer] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(3),
            Constraint::Length(3),
        ])
        .areas(f.area());
        let [left, right] =
            Layout::horizontal([Constraint::Length(34), Constraint::Min(0)]).areas(body);
        let [metrics, output] =
            Layout::vertical([Constraint::Length(7), Constraint::Min(0)]).areas(right);
        self.draw_header(f, header);
        self.draw_settings(f, left);
        self.draw_metrics(f, metrics);
        self.draw_output(f, output);
        let input_style = if self.focus == Focus::Input {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        };
        f.render_widget(
            Paragraph::new(self.input.as_str())
                .style(input_style)
                .block(Block::bordered().title(" message (Tab switches focus) ")),
            input,
        );
        self.draw_footer(f, footer);
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let tabs = self
            .chats
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if i == self.chat_idx {
                    format!("[{}]", c.title)
                } else {
                    c.title.clone()
                }
            })
            .collect::<Vec<_>>()
            .join("  ");
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    " ask chat ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::raw(tabs),
            ]))
            .block(Block::bordered()),
            area,
        );
    }

    fn draw_settings(&self, f: &mut Frame, area: Rect) {
        let items = ROWS
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let marker = if self.focus == Focus::Settings && i == self.selected {
                    "▸ "
                } else {
                    "  "
                };
                ListItem::new(Line::from(vec![
                    Span::raw(format!("{marker}{name:<11}")),
                    Span::styled(self.value(i), Style::default().fg(Color::Green)),
                ]))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default();
        state.select(Some(self.selected));
        f.render_stateful_widget(
            List::new(items).block(Block::bordered().title(" settings ")),
            area,
            &mut state,
        );
    }

    fn draw_metrics(&self, f: &mut Frame, area: Rect) {
        let mut lines = vec![Line::from(format!("material: {}", self.engine.news_note))];
        if let Some(res) = self.last_result() {
            let u = &res.outcome.usage;
            lines.push(Line::from(format!(
                "finish: {} · {} ms",
                res.outcome.finish_reason.as_deref().unwrap_or("?"),
                res.outcome.latency_ms
            )));
            lines.push(Line::from(format!(
                "tokens: prompt {} · completion {} · reasoning {}",
                u.prompt_tokens, u.completion_tokens, u.reasoning_tokens
            )));
            lines.push(Line::from(format!(
                "JSON: {} · schema: {}",
                yesno(res.json_ok()),
                yesno(res.schema_ok())
            )));
        } else {
            lines.push(Line::from("No messages yet"));
        }
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::bordered().title(" result ")),
            area,
        );
    }

    fn draw_output(&self, f: &mut Frame, area: Rect) {
        let (title, body) = if let Some(report) = &self.report {
            (" baseline vs strict JSON ", report.clone())
        } else {
            let mut body = String::new();
            for turn in &self.chats[self.chat_idx].turns {
                body.push_str(&format!("YOU\n{}\n\n", turn.question));
                if self.show_raw {
                    body.push_str(
                        &serde_json::to_string_pretty(&turn.result.outcome.raw).unwrap_or_default(),
                    );
                } else {
                    if let Some(reasoning) = turn
                        .result
                        .outcome
                        .reasoning
                        .as_deref()
                        .filter(|s| !s.trim().is_empty())
                    {
                        body.push_str(&format!("MODEL · THINKING\n{}\n\n", reasoning.trim()));
                    }
                    body.push_str("MODEL · FINAL\n");
                    if let Some(json) = &turn.result.json {
                        body.push_str(&serde_json::to_string_pretty(json).unwrap_or_default());
                    } else {
                        body.push_str(turn.result.outcome.text());
                    }
                }
                body.push_str("\n\n────────────────────────\n\n");
            }
            (
                if self.show_raw {
                    " raw conversation "
                } else {
                    " conversation: thinking + final "
                },
                body,
            )
        };
        f.render_widget(
            Paragraph::new(body)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0))
                .block(Block::bordered().title(title)),
            area,
        );
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let keys = "Enter send · Tab input/settings · Ctrl-N new · Ctrl-←/→ chats · c compare · r raw · PgUp/PgDn · Esc quit";
        f.render_widget(
            Paragraph::new(vec![
                Line::styled(self.status.clone(), Style::default().fg(Color::Cyan)),
                Line::styled(keys, Style::default().fg(Color::DarkGray)),
            ]),
            area,
        );
    }
}

fn format_tokens(n: u32) -> String {
    if n.is_multiple_of(1024) {
        format!("{}k", n / 1024)
    } else {
        n.to_string()
    }
}
fn yesno(v: bool) -> &'static str {
    if v {
        "yes"
    } else {
        "no"
    }
}
fn stop_index(cfg: &RunConfig) -> usize {
    STOP_CHOICES
        .iter()
        .position(|c| c.iter().map(|s| s.to_string()).collect::<Vec<_>>() == cfg.stop)
        .unwrap_or(0)
}
fn summarize(res: &RunResult) -> String {
    let mut s = format!(
        "finish={}",
        res.outcome.finish_reason.as_deref().unwrap_or("?")
    );
    if res.outcome.truncated() {
        s.push_str(" · token budget exhausted; answer may be incomplete");
    } else if res.json_ok() {
        s.push_str(" · required JSON completed");
    }
    s
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
fn step_opt(current: Option<u32>, delta: i32, min: u32, max: u32) -> Option<u32> {
    match current {
        None if delta > 0 => Some(min.max(delta as u32)),
        None => None,
        Some(v) => {
            let next = v as i32 + delta;
            if next < min as i32 {
                None
            } else {
                Some((next as u32).min(max))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn token_choices_are_requested_fixed_budgets() {
        assert_eq!(
            TOKEN_CHOICES,
            &[None, Some(8192), Some(16384), Some(33792), Some(65536)]
        );
    }
}
