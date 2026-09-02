//! Interactive settings screen. It owns no logic of its own: it edits the same
//! `RunConfig` the CLI builds and calls the same `Engine`.

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::time::Duration;

use crate::cli::display_stops;
use crate::compare;
use crate::config::{apply_preset, preset_names, Res, RunConfig, PRESETS};
use crate::engine::{Engine, RunResult};

/// Stop-sequence choices offered by the screen, in cycle order.
const STOP_CHOICES: &[&[&str]] = &[
    &[],
    &["\n---\n"],
    &["\n\n"],
    &["\n\n\n"],
    &["}"],
    &["\"items\""],
];

const ROWS: &[&str] = &[
    "preset",
    "format",
    "thinking",
    "max_tokens",
    "max_items",
    "stop",
    "runs",
    "source",
    "since (h)",
    "limit",
];

pub fn run(cfg: RunConfig) -> Res<()> {
    // Fetch before taking over the screen, so network errors are readable.
    let engine = Engine::new(&cfg)?;
    let mut app = App::new(cfg, engine);

    let mut terminal = ratatui::init();
    let result = app.event_loop(&mut terminal);
    ratatui::restore();
    result
}

enum View {
    Answer(Box<RunResult>),
    Report(String),
    Empty,
}

struct App {
    cfg: RunConfig,
    engine: Engine,
    selected: usize,
    stop_idx: usize,
    preset_idx: usize,
    view: View,
    status: String,
    scroll: u16,
    show_raw: bool,
    /// Set when a source/window change means the fetched material is out of date.
    news_dirty: bool,
    quit: bool,
}

impl App {
    fn new(cfg: RunConfig, engine: Engine) -> App {
        let stop_idx = STOP_CHOICES
            .iter()
            .position(|c| c.iter().map(|s| s.to_string()).collect::<Vec<_>>() == cfg.stop)
            .unwrap_or(0);
        App {
            preset_idx: matching_preset_idx(&cfg).unwrap_or(0),
            cfg,
            engine,
            selected: 0,
            stop_idx,
            view: View::Empty,
            status: "Enter — run · c — compare all presets · q to quit".into(),
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
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
                KeyCode::Up | KeyCode::Char('k') => {
                    self.selected = (self.selected + ROWS.len() - 1) % ROWS.len();
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.selected = (self.selected + 1) % ROWS.len();
                }
                KeyCode::Left | KeyCode::Char('h') => self.adjust(-1),
                KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') => self.adjust(1),
                KeyCode::Char('r') => {
                    self.show_raw = !self.show_raw;
                    self.scroll = 0;
                }
                KeyCode::Char('f') => self.refetch(terminal),
                KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
                KeyCode::PageDown => self.scroll = self.scroll.saturating_add(10),
                KeyCode::Home => self.scroll = 0,
                KeyCode::Enter => self.generate(terminal),
                KeyCode::Char('c') => self.run_compare(terminal),
                _ => {}
            }
        }
        Ok(())
    }

    // ---- settings -------------------------------------------------------

    fn value(&self, row: usize) -> String {
        match row {
            0 => matching_preset_idx(&self.cfg)
                .map(|i| PRESETS[i].name.to_string())
                .unwrap_or_else(|| "custom".into()),
            1 => self.cfg.format.to_string(),
            2 => if self.cfg.thinking { "on" } else { "off" }.into(),
            3 => self
                .cfg
                .max_tokens
                .map(|n| n.to_string())
                .unwrap_or_else(|| "off".into()),
            4 => self
                .cfg
                .max_items
                .map(|n| n.to_string())
                .unwrap_or_else(|| "off".into()),
            5 => display_stops(&self.cfg.stop),
            6 => self.cfg.runs.to_string(),
            7 => self.cfg.source.label().to_string(),
            8 => self.cfg.since_hours.to_string(),
            9 => self.cfg.limit.to_string(),
            _ => String::new(),
        }
    }

    fn adjust(&mut self, delta: i32) {
        match self.selected {
            0 => {
                let n = PRESETS.len() as i32;
                self.preset_idx = (self.preset_idx as i32 + delta).rem_euclid(n) as usize;
                let name = PRESETS[self.preset_idx].name;
                let _ = apply_preset(&mut self.cfg, name);
                self.stop_idx = STOP_CHOICES
                    .iter()
                    .position(|c| {
                        c.iter().map(|s| s.to_string()).collect::<Vec<_>>() == self.cfg.stop
                    })
                    .unwrap_or(0);
                self.status = format!("preset `{name}`: {}", PRESETS[self.preset_idx].blurb);
            }
            1 => self.cfg.format = self.cfg.format.cycle(delta),
            2 => self.cfg.thinking = !self.cfg.thinking,
            3 => self.cfg.max_tokens = step_opt(self.cfg.max_tokens, delta * 50, 50, 4000),
            4 => {
                self.cfg.max_items =
                    step_opt(self.cfg.max_items.map(|n| n as u32), delta, 1, 30).map(|n| n as usize)
            }
            5 => {
                let n = STOP_CHOICES.len() as i32;
                self.stop_idx = (self.stop_idx as i32 + delta).rem_euclid(n) as usize;
                self.cfg.stop = STOP_CHOICES[self.stop_idx]
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
            }
            6 => self.cfg.runs = (self.cfg.runs as i32 + delta).clamp(1, 20) as u32,
            7 => {
                self.cfg.source = self.cfg.source.cycle(delta);
                self.news_dirty = true;
            }
            8 => {
                self.cfg.since_hours =
                    (self.cfg.since_hours as i64 + delta as i64 * 12).clamp(6, 24 * 60) as u64;
                self.news_dirty = true;
            }
            9 => {
                self.cfg.limit = (self.cfg.limit as i32 + delta).clamp(1, 60) as usize;
                self.news_dirty = true;
            }
            _ => {}
        }
    }

    // ---- actions --------------------------------------------------------

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
        if self.news_dirty {
            self.refetch(terminal);
        }
        let _ = terminal.draw(|f| draw_busy(f, "generating…"));
        match self.engine.run_once(&self.cfg) {
            Ok(res) => {
                self.status = summarize(&res);
                self.view = View::Answer(Box::new(res));
                self.scroll = 0;
            }
            Err(e) => self.status = format!("request failed: {e}"),
        }
    }

    fn run_compare(&mut self, terminal: &mut DefaultTerminal) {
        if self.news_dirty {
            self.refetch(terminal);
        }
        let presets: Vec<String> = preset_names().iter().map(|s| s.to_string()).collect();
        let out_dir = compare::default_out_dir();
        let cfg = self.cfg.clone();

        let result = {
            let engine = &self.engine;
            compare::compare(engine, &cfg, &presets, Some(&out_dir), |name, i, n| {
                let _ = terminal.draw(|f| {
                    draw_busy(f, &format!("comparing `{name}` — run {i}/{n}"));
                });
            })
        };

        match result {
            Ok(reports) => {
                let text = compare::render_report(&self.engine, &cfg, &reports);
                let _ = std::fs::write(out_dir.join("report.md"), &text);
                self.status = format!("comparison saved to {}", out_dir.display());
                self.view = View::Report(text);
                self.scroll = 0;
            }
            Err(e) => self.status = format!("comparison failed: {e}"),
        }
    }

    // ---- rendering ------------------------------------------------------

    fn draw(&self, f: &mut Frame) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .areas(f.area());
        let [left, right] =
            Layout::horizontal([Constraint::Length(34), Constraint::Min(0)]).areas(body);
        let [metrics, output] =
            Layout::vertical([Constraint::Length(8), Constraint::Min(0)]).areas(right);

        self.draw_header(f, header);
        self.draw_settings(f, left);
        self.draw_metrics(f, metrics);
        self.draw_output(f, output);
        self.draw_footer(f, footer);
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let line = Line::from(vec![
            Span::styled(
                format!(" {} ", self.engine.topic.map(|t| t.title).unwrap_or("ask")),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(self.cfg.constraints(), Style::default().fg(Color::Yellow)),
        ]);
        f.render_widget(
            Paragraph::new(line).block(Block::bordered().title(" ask — response control ")),
            area,
        );
    }

    fn draw_settings(&self, f: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = ROWS
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let selected = i == self.selected;
                let marker = if selected { "▸ " } else { "  " };
                let style = if selected {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{marker}{name:<11}"), style),
                    Span::styled(self.value(i), Style::default().fg(Color::Green)),
                ]))
            })
            .collect();

        let mut state = ListState::default();
        state.select(Some(self.selected));
        f.render_stateful_widget(
            List::new(items).block(Block::bordered().title(" settings (←/→ to change) ")),
            area,
            &mut state,
        );
    }

    fn draw_metrics(&self, f: &mut Frame, area: Rect) {
        let mut lines = vec![Line::from(format!("material: {}", self.engine.news_note))];
        if self.news_dirty {
            lines.push(Line::styled(
                "source changed — press f to refetch",
                Style::default().fg(Color::Yellow),
            ));
        }
        match &self.view {
            View::Answer(res) => {
                let u = &res.outcome.usage;
                lines.push(Line::from(format!(
                    "finish: {}   latency: {} ms",
                    res.outcome.finish_reason.as_deref().unwrap_or("?"),
                    res.outcome.latency_ms
                )));
                lines.push(Line::from(format!(
                    "tokens: prompt {} · completion {} · reasoning {}",
                    u.prompt_tokens, u.completion_tokens, u.reasoning_tokens
                )));
                lines.push(status_line(
                    "JSON parses",
                    res.json_ok(),
                    self.cfg.format.expects_json(),
                ));
                lines.push(status_line("schema matches", res.schema_ok(), true));
                if let Some(n) = res.item_count() {
                    lines.push(Line::from(format!("items in answer: {n}")));
                }
                if let Some(e) = &res.json_error {
                    lines.push(Line::styled(
                        format!("json error: {e}"),
                        Style::default().fg(Color::Red),
                    ));
                }
            }
            View::Report(_) => lines.push(Line::from("comparison report in the panel below")),
            View::Empty => lines.push(Line::from("press Enter to generate")),
        }
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::bordered().title(" result ")),
            area,
        );
    }

    fn draw_output(&self, f: &mut Frame, area: Rect) {
        let (title, body) = match &self.view {
            View::Answer(res) => {
                let text = if self.show_raw {
                    serde_json::to_string_pretty(&res.outcome.raw).unwrap_or_default()
                } else if let Some(v) = &res.json {
                    serde_json::to_string_pretty(v).unwrap_or_default()
                } else {
                    let t = res.outcome.text().to_string();
                    if t.is_empty() {
                        "(no content — generation stopped before any visible token)".into()
                    } else {
                        t
                    }
                };
                (
                    if self.show_raw {
                        " raw response (r) "
                    } else {
                        " answer (r for raw) "
                    },
                    text,
                )
            }
            View::Report(text) => (" comparison ", text.clone()),
            View::Empty => (" answer ", String::new()),
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
        let keys = "↑↓ select · ←→ change · Enter run · c compare · f refetch · r raw · PgUp/PgDn scroll · q quit";
        let lines = vec![
            Line::styled(self.status.clone(), Style::default().fg(Color::Cyan)),
            Line::styled(keys, Style::default().fg(Color::DarkGray)),
        ];
        f.render_widget(Paragraph::new(lines).block(Block::bordered()), area);
    }
}

fn status_line(label: &str, ok: bool, applicable: bool) -> Line<'static> {
    if !applicable {
        return Line::from(format!("{label}: n/a"));
    }
    let (mark, color) = if ok {
        ("yes", Color::Green)
    } else {
        ("no", Color::Red)
    };
    Line::from(vec![
        Span::raw(format!("{label}: ")),
        Span::styled(mark, Style::default().fg(color)),
    ])
}

fn summarize(res: &RunResult) -> String {
    let mut parts = vec![format!(
        "finish={}",
        res.outcome.finish_reason.as_deref().unwrap_or("?")
    )];
    if res.outcome.truncated() {
        parts.push("truncated by max_tokens".into());
    }
    if let Some(e) = &res.json_error {
        parts.push(format!("json error: {e}"));
    } else if !res.schema_errors.is_empty() {
        parts.push(format!("{} schema violation(s)", res.schema_errors.len()));
    }
    parts.join(" · ")
}

fn draw_busy(f: &mut Frame, msg: &str) {
    let area = f.area();
    let block = Block::bordered().title(" working ");
    f.render_widget(
        Paragraph::new(msg).block(block),
        Rect {
            x: area.x,
            y: area.y + area.height / 2,
            width: area.width,
            height: 3.min(area.height),
        },
    );
}

/// Which named preset, if any, matches the knobs currently on `cfg`.
fn matching_preset_idx(cfg: &RunConfig) -> Option<usize> {
    PRESETS.iter().position(|p| {
        let mut probe = RunConfig::default();
        let _ = apply_preset(&mut probe, p.name);
        probe.format == cfg.format
            && probe.max_tokens == cfg.max_tokens
            && probe.max_items == cfg.max_items
            && probe.stop == cfg.stop
            && probe.thinking == cfg.thinking
    })
}

/// `off -> min -> …` stepping used by the numeric rows, where 0 means "not set".
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
