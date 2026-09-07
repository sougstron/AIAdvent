//! Argument parsing and the non-interactive entry points: one-shot questions,
//! `--sessions`, and `--verify-stop` (the stop-condition proof from a script).

use clap::Parser;
use serde_json::Value;
use std::io::{IsTerminal, Read};

use crate::agent::Agent;
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
        ask --sessions                        list saved chat sessions\n  \
        ask --resume ID                       resume a saved session\n  \
        ask --continue                        resume the most recent session"
)]
pub struct Cli {
    /// Question for a one-shot answer. With none, opens the chat TUI.
    #[arg(trailing_var_arg = true)]
    pub question: Vec<String>,

    /// Model id from the z.ai catalog. Only glm-5.3-flash may be called live.
    #[arg(long)]
    pub model: Option<String>,

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
    /// Sent as `max_tokens`.
    #[arg(long, env = "ASK_BUDGET_TOKENS")]
    pub budget_tokens: Option<u32>,

    /// Alias for --budget-tokens: cap on generated tokens.
    #[arg(long, env = "ASK_MAX_TOKENS")]
    pub max_tokens: Option<u32>,

    /// Stop sequence; repeatable, max 4. `\n`/`\t` escapes are understood.
    #[arg(long = "stop", env = "ASK_STOP", value_delimiter = ',')]
    pub stop: Vec<String>,

    /// Sampling temperature, 0.0 to 1.0 (z.ai documented range).
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

    /// Resume a saved session by id (TUI, or one-shot if a question is given).
    #[arg(long)]
    pub resume: Option<String>,

    /// Resume the most recently updated session.
    #[arg(long = "continue")]
    pub continue_last: bool,

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
        if let Some(m) = &self.model {
            if !config::MODEL_CATALOG.contains(&m.as_str()) {
                return Err(format!(
                    "unknown model `{m}`; catalog: {}",
                    config::MODEL_CATALOG.join(", ")
                ));
            }
            s.model = m.clone();
        }
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
        if let Some(n) = self.max_tokens.or(self.budget_tokens) {
            s.budget_tokens = Some(config::parse_max_tokens(n)?);
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
            s.top_p = Some(config::parse_top_p(p)?);
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
        let prompt = if cli.question.is_empty() {
            verify::DEFAULT_PROMPT.to_string()
        } else {
            cli.question.join(" ")
        };
        let report = verify::run(&settings, &prompt)?;
        println!("{}", report.render());
        return Ok(());
    }

    let dir = session::sessions_dir();
    let loaded = if let Some(id) = cli.resume.as_deref() {
        Some(session::load_session(&dir, id)?)
    } else if cli.continue_last {
        Some(session::continue_last(&dir)?)
    } else {
        None
    };

    let question = read_question(&cli.question)?;
    if question.is_empty() {
        return crate::tui::run(settings, loaded);
    }

    one_shot(&cli, settings, &question, loaded)
}

fn one_shot(
    cli: &Cli,
    settings: Settings,
    question: &str,
    loaded: Option<session::Session>,
) -> Res<()> {
    let mut agent = Agent::new(settings.clone())?;
    let mut sess = if let Some(s) = loaded {
        agent.resume(&s);
        s
    } else {
        session::Session::new(agent.settings().clone())
    };
    let reply = agent.ask(question)?;

    if cli.raw {
        println!("{}", serde_json::to_string_pretty(&reply.raw).unwrap_or_default());
    } else {
        let (display, parse_note) = if settings.json_mode.enabled {
            render::render_json_reply(&reply.text)
        } else {
            (reply.text.clone(), None)
        };
        if display.is_empty() {
            println!("(no content)");
        } else {
            println!("{display}");
        }
        if reply.truncated_by_max_chars {
            eprintln!("! truncated to max_chars={}", settings.max_chars.unwrap_or(0));
        }
        if let Some(e) = parse_note {
            eprintln!("! {e}");
        }
    }

    if !cli.quiet {
        let u = &reply.usage;
        eprintln!(
            "« model={}  finish={}  tokens: prompt={} completion={} (reasoning={}) total={}  {}ms  |  {}",
            if reply.model.is_empty() { "?" } else { &reply.model },
            reply.finish_reason.as_deref().unwrap_or("?"),
            u.prompt_tokens,
            u.completion_tokens,
            u.reasoning_tokens,
            u.total_tokens,
            reply.latency_ms,
            agent.settings().summary(),
        );
        for f in agent.context_files() {
            eprintln!(
                "« context {}: {} ({} chars{})",
                f.scope.as_str(),
                f.path.display(),
                f.chars,
                if f.truncated { ", truncated" } else { "" }
            );
        }
    }
    if reply.truncated() {
        eprintln!("! cut by max_tokens — the answer may be incomplete by construction");
    }
    if reply.stopped_by_sequence() && !settings.stop.is_empty() {
        eprintln!("! stopped on a stop sequence");
    }
    if let Some(r) = reply.reasoning.as_deref().filter(|s| !s.trim().is_empty()) {
        eprintln!("« reasoning: {} chars", r.trim().chars().count());
    }
    sess.capture_from(
        agent.settings(),
        agent
            .context_files()
            .iter()
            .map(|f| f.path.to_string_lossy().into_owned()),
        agent.history(),
    );
    if let Err(e) = sess.save(&session::sessions_dir()) {
        eprintln!("warning: could not save session: {e}");
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
        println!("{}", s.line());
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
