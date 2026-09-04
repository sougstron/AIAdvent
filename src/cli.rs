//! Argument parsing and the non-interactive entry points: one-shot questions,
//! `--sessions`, and `--verify-stop` (the stop-condition proof from a script).

use clap::Parser;
use serde_json::Value;
use std::io::{IsTerminal, Read};

use crate::api::{self, ChatMessage, Endpoint};
use crate::config::{self, Effort, JsonMode, Res, Settings};
use crate::render;
use crate::session;
use crate::verify;

#[derive(Parser, Debug)]
#[command(
    name = "ask",
    about = "A ChatGPT-style chat client for the terminal, over an OpenAI-compatible endpoint",
    long_about = "With no arguments, opens the interactive chat TUI: sessions, reasoning \
                  effort, JSON-schema output, a character length cap and a verifiable stop \
                  condition. With a question, sends it once and prints the answer.",
    after_help = "Examples:\n  \
        ask                                   open the chat TUI\n  \
        ask \"what is the capital of France?\"   one-shot question\n  \
        ask --verify-stop                     prove the stop condition works\n  \
        ask --sessions                        list saved chat sessions"
)]
pub struct Cli {
    /// Question for a one-shot answer. With none, opens the chat TUI.
    #[arg(trailing_var_arg = true)]
    pub question: Vec<String>,

    /// Reasoning effort: none | low | medium | high.
    #[arg(long, env = "ASK_EFFORT")]
    pub effort: Option<String>,

    /// Turn on structured JSON output for this one-shot call.
    #[arg(long)]
    pub json: bool,

    /// Flat schema for --json: comma-separated string field names.
    #[arg(long, value_delimiter = ',')]
    pub json_fields: Vec<String>,

    /// Full JSON Schema file for --json, overriding --json-fields.
    #[arg(long)]
    pub json_schema_file: Option<String>,

    /// Hard character cap on the visible answer. Enforced client-side.
    #[arg(long, env = "ASK_MAX_CHARS")]
    pub max_chars: Option<usize>,

    /// Stop-condition token budget (counts reasoning + visible tokens).
    #[arg(long, env = "ASK_BUDGET_TOKENS")]
    pub budget_tokens: Option<u32>,

    /// Stop sequence; repeatable, max 4. `\n`/`\t` escapes are understood.
    #[arg(long = "stop", env = "ASK_STOP", value_delimiter = ',')]
    pub stop: Vec<String>,

    /// Sampling temperature, 0.0 to 2.0 (the provider's own range).
    #[arg(long)]
    pub temperature: Option<f32>,

    /// Nucleus sampling cutoff, 0.0 to 1.0. Unset: provider default.
    #[arg(long)]
    pub top_p: Option<f32>,

    /// Top-k cutoff; -1 disables it. Unset: provider default (which damps
    /// the visible effect of --temperature).
    // `allow_hyphen_values` so `--top-k -1` reads as a value, not a flag.
    #[arg(long, allow_hyphen_values = true)]
    pub top_k: Option<i32>,

    /// Run the stop-condition proof (same prompt, off vs on) and exit.
    #[arg(long)]
    pub verify_stop: bool,

    /// List saved chat sessions and exit.
    #[arg(long)]
    pub sessions: bool,

    /// Print the full API response instead of just the answer.
    #[arg(long)]
    pub raw: bool,

    /// Never print the usage/finish_reason line.
    #[arg(long)]
    pub quiet: bool,
}

impl Cli {
    pub fn to_settings(&self) -> Res<Settings> {
        let mut s = Settings::default();
        if let Some(e) = &self.effort {
            s.effort = Effort::parse(e)?;
        }
        if let Some(path) = &self.json_schema_file {
            let raw = std::fs::read_to_string(path)
                .map_err(|e| format!("cannot read {path}: {e}"))?;
            let schema: Value =
                serde_json::from_str(&raw).map_err(|e| format!("cannot parse {path}: {e}"))?;
            s.json_mode = JsonMode {
                enabled: true,
                schema,
            };
        } else if !self.json_fields.is_empty() {
            s.json_mode = JsonMode {
                enabled: true,
                schema: config::flat_string_schema(&self.json_fields),
            };
        } else if self.json {
            s.json_mode.enabled = true;
        }
        if let Some(n) = self.max_chars {
            s.max_chars = Some(n);
        }
        if let Some(n) = self.budget_tokens {
            s.budget_tokens = Some(n);
        }
        if !self.stop.is_empty() {
            if self.stop.len() > 4 {
                return Err("at most 4 stop sequences are supported".into());
            }
            s.stop = self.stop.iter().map(|s| config::unescape(s)).collect();
        }
        if let Some(t) = self.temperature {
            s.temperature = Some(config::parse_temperature(t)?);
        }
        if let Some(p) = self.top_p {
            if !(0.0..=1.0).contains(&p) {
                return Err(format!("top_p must be between 0.0 and 1.0 (got {p})"));
            }
            s.top_p = Some(p);
        }
        if let Some(k) = self.top_k {
            if k == 0 || k < -1 {
                return Err(format!("top_k must be -1 (off) or a positive count (got {k})"));
            }
            s.top_k = Some(k);
        }
        Ok(s)
    }
}

pub fn run() -> Res<()> {
    let cli = Cli::parse();

    if cli.sessions {
        return print_sessions();
    }

    let settings = cli.to_settings()?;

    if cli.verify_stop {
        let ep = Endpoint::resolve()?;
        let prompt = if cli.question.is_empty() {
            verify::DEFAULT_PROMPT.to_string()
        } else {
            cli.question.join(" ")
        };
        let report = verify::run(&ep, &settings, &prompt)?;
        println!("{}", report.render());
        return Ok(());
    }

    let question = read_question(&cli.question)?;
    if question.is_empty() {
        return crate::tui::run(settings);
    }

    one_shot(&cli, settings, &question)
}

fn one_shot(cli: &Cli, settings: Settings, question: &str) -> Res<()> {
    let ep = Endpoint::resolve()?;
    let history = vec![ChatMessage::user(question)];
    let schema = settings.json_mode.enabled.then(|| settings.json_mode.schema.clone());
    let outcome = api::chat(&ep, &settings, "", &history, schema.as_ref())?;

    if cli.raw {
        println!("{}", serde_json::to_string_pretty(&outcome.raw).unwrap_or_default());
    } else {
        let raw_text = outcome.text();
        let (display, parse_note) = if settings.json_mode.enabled {
            render::render_json_reply(raw_text)
        } else {
            (raw_text.to_string(), None)
        };
        let (capped, was_cut) = api::enforce_max_chars(&display, settings.max_chars);
        if capped.is_empty() {
            println!("(no content)");
        } else {
            println!("{capped}");
        }
        if was_cut {
            eprintln!("! truncated to max_chars={}", settings.max_chars.unwrap_or(0));
        }
        if let Some(e) = parse_note {
            eprintln!("! {e}");
        }
    }

    if !cli.quiet {
        let u = &outcome.usage;
        eprintln!(
            "« finish={}  tokens: prompt={} completion={} (reasoning={}) total={}  {}ms  |  {}",
            outcome.finish_reason.as_deref().unwrap_or("?"),
            u.prompt_tokens,
            u.completion_tokens,
            u.reasoning_tokens,
            u.total_tokens,
            outcome.latency_ms,
            settings.summary(),
        );
    }
    if outcome.truncated() {
        eprintln!("! cut by budget_tokens — the answer may be incomplete by construction");
    }
    if let Some(r) = outcome.reasoning.as_deref().filter(|s| !s.trim().is_empty()) {
        eprintln!("« reasoning: {} chars", r.trim().chars().count());
    }
    Ok(())
}

fn print_sessions() -> Res<()> {
    let dir = session::sessions_dir();
    let list = session::list_sessions(&dir);
    if list.is_empty() {
        println!("no saved sessions in {}", dir.display());
        return Ok(());
    }
    for s in list {
        println!("{:<20} {:<32} {} msgs", s.id, s.title, s.message_count);
    }
    Ok(())
}

/// Positional args, or stdin when it is piped in. Empty (and a TTY) means
/// "open the chat TUI" — same convention as before.
fn read_question(args: &[String]) -> Res<String> {
    if !args.is_empty() {
        return Ok(args.join(" "));
    }
    if std::io::stdin().is_terminal() {
        return Ok(String::new());
    }
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| format!("cannot read stdin: {e}"))?;
    Ok(buf.trim().to_string())
}
