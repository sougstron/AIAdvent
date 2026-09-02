//! Argument parsing and the non-interactive command flow.

use clap::Parser;
use std::io::{IsTerminal, Read};

use crate::compare;
use crate::config::{
    self, apply_preset, preset_names, render_stops, Format, Res, RunConfig, Source,
};
use crate::engine::{Engine, RunResult};
use crate::topics;

#[derive(Parser, Debug)]
#[command(
    name = "ask",
    about = "Ask an LLM — with explicit control over the answer's format, length and stopping",
    long_about = "Sends a question to an OpenAI-compatible endpoint with three independent \
                  response controls: output format (text / json_object / strict json_schema), \
                  a length limit (token budget or schema maxItems) and stop sequences.\n\n\
                  With no question it acts as a game-development news service: it fetches real \
                  news from public APIs and returns a digest in the requested format.",
    after_help = "Examples:\n  \
        ask \"what is the capital of France?\"          plain question, unconstrained\n  \
        ask --preset strict                           digest as strict JSON\n  \
        ask --compare --runs 5                        run every preset and report stability\n  \
        ask --tui                                     interactive switches"
)]
pub struct Cli {
    /// Question, or an extra focus hint for the topic digest.
    #[arg(trailing_var_arg = true)]
    pub question: Vec<String>,

    /// Topic/schema to use: gamedev, or `none` for a plain pass-through question.
    #[arg(long, env = "ASK_TOPIC")]
    pub topic: Option<String>,

    /// Answer format: text | json | schema.
    #[arg(long, env = "ASK_FORMAT")]
    pub format: Option<String>,

    /// Start from a named preset, then apply any other flags on top.
    #[arg(long, env = "ASK_PRESET")]
    pub preset: Option<String>,

    /// Hard generation budget. Note: it also counts the model's reasoning tokens.
    #[arg(long, env = "ASK_MAX_TOKENS")]
    pub max_tokens: Option<u32>,

    /// Semantic length limit: caps the number of entries via the schema's maxItems.
    #[arg(long, env = "ASK_MAX_ITEMS")]
    pub max_items: Option<usize>,

    /// Stop sequence; repeatable. `\n` and `\t` escapes are understood.
    #[arg(long = "stop", env = "ASK_STOP", value_delimiter = ',')]
    pub stop: Vec<String>,

    /// Model reasoning: on | off. Off by default — reasoning breaks length/stop control.
    #[arg(long, env = "ASK_THINK")]
    pub think: Option<String>,

    /// Repeat the request N times (stability checking).
    #[arg(long, env = "ASK_RUNS")]
    pub runs: Option<u32>,

    /// Sampling temperature.
    #[arg(long, env = "ASK_TEMPERATURE")]
    pub temperature: Option<f32>,

    /// Use a JSON schema from this file instead of the topic's built-in one.
    #[arg(long, env = "ASK_SCHEMA_FILE")]
    pub schema_file: Option<String>,

    /// News source: steam | hn | both | none.
    #[arg(long, env = "ASK_SOURCE")]
    pub source: Option<String>,

    /// Time window for the news fetch, e.g. 48h or 7d (bare number = hours).
    #[arg(long, env = "ASK_SINCE")]
    pub since: Option<String>,

    /// Maximum number of source items fed to the model.
    #[arg(long, env = "ASK_LIMIT")]
    pub limit: Option<usize>,

    /// Run every preset and print the stability comparison.
    /// Defaults to 5 runs per preset unless `--runs` is set.
    #[arg(long)]
    pub compare: bool,

    /// Presets to compare (comma-separated). Defaults to all of them.
    #[arg(long, value_delimiter = ',')]
    pub presets: Vec<String>,

    /// Where to store the raw responses of a comparison. Default: runs/<timestamp>.
    #[arg(long)]
    pub out_dir: Option<String>,

    /// Do not save raw responses during a comparison.
    #[arg(long)]
    pub no_save: bool,

    /// Print the full API response instead of just the answer.
    #[arg(long)]
    pub raw: bool,

    /// Print the prompt that would be sent, and exit.
    #[arg(long)]
    pub show_prompt: bool,

    /// Print the request body that would be sent, and exit.
    #[arg(long)]
    pub show_request: bool,

    /// Always print the usage/finish_reason line on stderr.
    #[arg(long)]
    pub stats: bool,

    /// Never print the usage/finish_reason line.
    #[arg(long)]
    pub quiet: bool,

    /// Interactive settings screen.
    #[arg(long)]
    pub tui: bool,

    /// List the available presets and topics, then exit.
    #[arg(long)]
    pub list_presets: bool,
}

impl Cli {
    /// Resolution order: defaults -> preset -> explicit flags/env.
    pub fn to_config(&self) -> Res<RunConfig> {
        // --tui owns stdin for key events; do not drain it as a piped question.
        let question = if self.tui {
            self.question.join(" ")
        } else {
            read_question(&self.question)?
        };

        // A bare question keeps the original behaviour of this CLI; with no question we
        // are the news service, so the topic (and its schema) becomes the default.
        let topic = self.topic.clone().unwrap_or_else(|| {
            if question.is_empty() {
                "gamedev".into()
            } else {
                "none".into()
            }
        });
        if topic != "none" {
            topics::get(&topic)?;
        }

        let mut cfg = RunConfig {
            topic,
            question,
            ..Default::default()
        };
        if cfg.topic == "none" {
            cfg.format = Format::Text;
            cfg.source = Source::None;
            // Preserve the original unconstrained CLI: the model thinks unless asked not to.
            cfg.thinking = true;
        }

        if let Some(name) = &self.preset {
            apply_preset(&mut cfg, name)?;
        }

        if let Some(f) = &self.format {
            cfg.format = Format::parse(f)?;
        }
        if let Some(n) = self.max_tokens {
            cfg.max_tokens = Some(n);
        }
        if let Some(n) = self.max_items {
            cfg.max_items = Some(n);
        }
        if !self.stop.is_empty() {
            cfg.stop = self.stop.iter().map(|s| unescape(s)).collect();
        }
        if let Some(t) = &self.think {
            cfg.thinking = parse_switch(t)?;
        }
        if let Some(n) = self.runs {
            if n == 0 {
                return Err("--runs must be at least 1".into());
            }
            cfg.runs = n;
        }
        if let Some(t) = self.temperature {
            cfg.temperature = Some(t);
        }
        if let Some(p) = &self.schema_file {
            cfg.schema_file = Some(p.clone());
        }
        if let Some(s) = &self.source {
            cfg.source = Source::parse(s)?;
        }
        if let Some(s) = &self.since {
            cfg.since_hours = parse_duration_hours(s)?;
        }
        if let Some(n) = self.limit {
            cfg.limit = n;
        }

        if cfg.format == Format::JsonSchema && cfg.topic == "none" && cfg.schema_file.is_none() {
            return Err("--format schema needs a topic or --schema-file".into());
        }
        if cfg.stop.len() > 4 {
            return Err("at most 4 stop sequences are supported".into());
        }
        Ok(cfg)
    }

    fn wants_stats(&self, cfg: &RunConfig) -> bool {
        if self.quiet {
            return false;
        }
        self.stats
            || self.raw
            || cfg.runs > 1
            || cfg.topic != "none"
            || cfg.max_tokens.is_some()
            || !cfg.stop.is_empty()
            || cfg.format != Format::Text
    }
}

pub fn run() -> Res<()> {
    let cli = Cli::parse();

    if cli.list_presets {
        print_presets();
        return Ok(());
    }

    let mut cfg = cli.to_config()?;

    if cli.tui {
        return crate::tui::run(cfg);
    }

    // A single run cannot say anything about shape stability, so `--compare`
    // defaults to 5 unless the user picked a number themselves.
    if cli.compare && cli.runs.is_none() {
        cfg.runs = 5;
    }

    let engine = Engine::new(&cfg)?;

    if cli.show_prompt {
        let (system, user) = engine.prompt(&cfg);
        println!("--- system ---\n{system}\n\n--- user ---\n{user}");
        return Ok(());
    }
    if cli.show_request {
        let (system, user) = engine.prompt(&cfg);
        let schema = match (engine.topic, cfg.format) {
            (Some(t), Format::JsonSchema) => Some(t.schema(&cfg)?),
            _ => None,
        };
        let body = crate::api::build_body(
            &engine.endpoint.model,
            &cfg,
            &system,
            &user,
            schema.as_ref(),
        );
        println!(
            "{}",
            serde_json::to_string_pretty(&body).unwrap_or_default()
        );
        return Ok(());
    }

    if cli.compare {
        return run_compare(&cli, &cfg, &engine);
    }

    if cli.wants_stats(&cfg) {
        let topic_label = engine.topic.map(|t| t.title).unwrap_or("pass-through");
        eprintln!(
            "» {}  |  {}  |  material: {}",
            cfg.constraints(),
            topic_label,
            engine.news_note
        );
    }

    for i in 0..cfg.runs {
        if cfg.runs > 1 {
            println!("===== run {}/{} =====", i + 1, cfg.runs);
        }
        let res = engine.run_once(&cfg)?;
        print_result(&cli, &cfg, &res);
    }
    Ok(())
}

fn print_result(cli: &Cli, cfg: &RunConfig, res: &RunResult) {
    if cli.raw {
        println!(
            "{}",
            serde_json::to_string_pretty(&res.outcome.raw).unwrap_or_default()
        );
    } else if let Some(v) = &res.json {
        // Re-serialising makes runs visually comparable regardless of the model's spacing.
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
    } else {
        let text = res.outcome.text();
        if text.is_empty() {
            println!("(no content)");
        } else {
            println!("{text}");
        }
    }

    if cli.wants_stats(cfg) {
        let u = &res.outcome.usage;
        eprintln!(
            "« finish={}  tokens: prompt={} completion={} (reasoning={})  {}ms",
            res.outcome.finish_reason.as_deref().unwrap_or("?"),
            u.prompt_tokens,
            u.completion_tokens,
            u.reasoning_tokens,
            res.outcome.latency_ms,
        );
        if let Some(r) = res
            .outcome
            .reasoning
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            eprintln!("« reasoning: {} chars", r.chars().count());
        }
    }

    // The three failure modes worth naming explicitly, instead of printing quiet garbage.
    if res.outcome.truncated() {
        eprintln!(
            "! truncated by max_tokens={} — the answer is incomplete by construction",
            cfg.max_tokens
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".into())
        );
    }
    if res
        .outcome
        .content
        .as_deref()
        .unwrap_or("")
        .trim()
        .is_empty()
    {
        eprintln!(
            "! empty content: generation stopped before any visible token{}",
            if cfg.thinking {
                " (reasoning is on and consumed the budget)"
            } else {
                ""
            }
        );
    }
    if let Some(e) = &res.json_error {
        eprintln!("! not valid JSON: {e}");
    }
    for e in &res.schema_errors {
        eprintln!("! schema violation {e}");
    }
}

fn run_compare(cli: &Cli, cfg: &RunConfig, engine: &Engine) -> Res<()> {
    let presets: Vec<String> = if cli.presets.is_empty() {
        preset_names().iter().map(|s| s.to_string()).collect()
    } else {
        cli.presets.clone()
    };
    for p in &presets {
        config::preset(p)?;
    }

    let out_dir = if cli.no_save {
        None
    } else {
        Some(
            cli.out_dir
                .clone()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(compare::default_out_dir),
        )
    };

    eprintln!(
        "» comparing {} presets × {} runs  |  topic={}  |  material: {}",
        presets.len(),
        cfg.runs,
        cfg.topic,
        engine.news_note
    );

    let reports = compare::compare(engine, cfg, &presets, out_dir.as_deref(), |name, i, n| {
        eprint!("\r  {name}: run {i}/{n}          ");
    })?;
    eprintln!("\r{:60}\r", "");

    let report = compare::render_report(engine, cfg, &reports);
    println!("{report}");

    if let Some(dir) = &out_dir {
        let path = dir.join("report.md");
        std::fs::write(&path, &report)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        eprintln!("raw responses and report saved to {}", dir.display());
    }
    Ok(())
}

fn print_presets() {
    println!("Presets:");
    for p in config::PRESETS {
        let mut cfg = RunConfig::default();
        let _ = apply_preset(&mut cfg, p.name);
        println!("  {:<13} {}", p.name, p.blurb);
        println!("  {:<13} {}", "", cfg.constraints());
    }
    println!("\nTopics: {}, none", topics::names().join(", "));
    println!("Formats: text, json, schema");
}

/// Positional args, or stdin when it is piped in.
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

fn parse_switch(s: &str) -> Res<bool> {
    match s {
        "on" | "true" | "1" | "yes" => Ok(true),
        "off" | "false" | "0" | "no" => Ok(false),
        other => Err(format!("expected on|off, got `{other}`")),
    }
}

/// `48h`, `7d`, or a bare number meaning hours.
pub fn parse_duration_hours(s: &str) -> Res<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('h') | Some('H') => (&s[..s.len() - 1], 1),
        Some('d') | Some('D') => (&s[..s.len() - 1], 24),
        Some('w') | Some('W') => (&s[..s.len() - 1], 24 * 7),
        _ => (s, 1),
    };
    num.trim()
        .parse::<u64>()
        .map(|n| n * mult)
        .map_err(|_| format!("cannot parse duration `{s}` (use 48h, 7d, 2w)"))
}

/// Turns the literal two-character `\n` typed in a shell into a real newline.
pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Re-exported for the TUI, which shows the same escaped form.
pub fn display_stops(stop: &[String]) -> String {
    if stop.is_empty() {
        "off".into()
    } else {
        render_stops(stop)
    }
}
