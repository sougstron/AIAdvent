//! Argument parsing and the non-interactive entry points: one-shot questions,
//! `--sessions`, `--verify`, and the login flows (`--login`, `--keys`,
//! `--logout`, `--verify-login`) that manage the machine's own API keys.

use clap::Parser;
use serde_json::Value;
use std::io::{IsTerminal, Read};

use crate::api::{self, Endpoint};
use crate::auth::{self, CheckResult, Provider};
use crate::billing;
use crate::config::{self, Effort, JsonMode, Res, Settings};
use crate::isolation;
use crate::mcp;
use crate::memory;
use crate::profile;
use crate::render;
use crate::runtime::{BoxSpec, Runtime};
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
        ask --login                           connect an API key (glm/deepseek/openrouter)\n  \
        printf %s \"$KEY\" | ask --login glm --key-stdin   connect a key from a script\n  \
        ask --keys                            show configured keys and their sources\n  \
        ask --models                          catalog grouped by provider (live / paid / no key)\n  \
        ask --verify-login                    live-recheck every configured key\n  \
        ask --verify                          prove z.ai levers with causal signatures\n  \
        ask --verify-billing                  show whether spend is metered or plan quota\n  \
        ask --strategy summary                history compression: keep the last N, summarize the rest\n  \
        ask --verify-compress                 prove compression saves tokens without losing facts\n  \
        ask --verify-context all              prove window / facts / branch strategies live\n  \
        ask --strategy memory                 three memory layers: short / working / long\n  \
        ask --verify-memory all               prove the memory layers are separate and load-bearing\n  \
        ask --profile chemist \"...\"            answer through a user profile (style / format / limits)\n  \
        ask --profiles                        list the profile catalog\n  \
        ask --verify-profile all              prove the profile reaches the wire and changes the voice\n  \
        ask --todo \"...\"                      run the answer through the task state machine\n  \
        ask --verify-todo all                 prove the task state machine holds and survives a pause\n  \
        ask --verify-lifecycle all            prove the task lifecycle is gated: no execute before an approved plan\n  \
        ask --mcp-tools [URL]                 connect to an MCP server and list its tools (default: DeepWiki)\n  \
        ask --strategy window --keep-recent 6 send only the last N messages\n  \
        ask --sessions                        list saved chat sessions\n  \
        ask --resume ID                       resume a saved session\n  \
        ask --continue                        resume the most recent session"
)]
pub struct Cli {
    /// Question for a one-shot answer. With none, opens the chat TUI.
    #[arg(trailing_var_arg = true)]
    pub question: Vec<String>,

    /// Model id from the catalog. Its provider must be connected (env var or
    /// `ask --login`); the request goes to that provider's endpoint. Paid ids
    /// are refused at send time by the money guard.
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

    /// Context-management strategy: `off` (whole history), `summary`
    /// (running summary + last --keep-recent), `window` (last --keep-recent
    /// only), `facts` (key-value memory + last --keep-recent), `branch`
    /// (the active conversation branch) or `memory` (three memory layers in
    /// the system prompt + last --keep-recent). `--compress` is the old name.
    #[arg(
        long = "strategy",
        visible_alias = "compress",
        env = "ASK_COMPRESS",
        value_name = "STRATEGY"
    )]
    pub compress: Option<String>,

    /// With summary/window/facts: how many recent messages stay verbatim.
    #[arg(long, value_name = "N")]
    pub keep_recent: Option<usize>,

    /// With --strategy summary: how many messages one summary fold covers.
    #[arg(long, value_name = "N")]
    pub summarize_every: Option<usize>,

    /// Live proof that history compression saves tokens and keeps the facts
    /// from the folded part. Exits after printing.
    #[arg(long)]
    pub verify_compress: bool,

    /// Live proof for the other context strategies: `window`, `facts`,
    /// `branch` or `all`. Each verdict is a causal signature, not a text
    /// diff. Exits after printing.
    #[arg(long, value_name = "WHICH")]
    pub verify_context: Option<String>,

    /// Live proof for the three-layer memory model: `routing` (what goes
    /// into which layer, and what that leaves on disk), `influence` (one
    /// layer at a time, on the answers), `isolation` (switching tasks) or
    /// `all`. With --offline, `routing` skips its live half. Exits after
    /// printing.
    #[arg(long, value_name = "WHICH")]
    pub verify_memory: Option<String>,

    /// User profile applied to every request: an id from the catalog
    /// (`--profiles`) or `off`. The profile carries style, format and
    /// limits, and replaces the default "helpful assistant" prompt while
    /// that prompt is still the default one.
    #[arg(long, value_name = "ID")]
    pub profile: Option<String>,

    /// List the profile catalog (built-ins plus your own) and exit.
    #[arg(long)]
    pub profiles: bool,

    /// Task state machine (`todo.rs`): run the answer through the
    /// study → plan → execute → validate → report ladder instead of one
    /// plain turn. Off by default.
    #[arg(long)]
    pub todo: bool,

    /// Who signs off the plan before implementation starts: `manual`
    /// (a human runs `/todo approve`) or `auto` (signed automatically and
    /// logged as `approved-by=auto`). The gate itself is never skipped.
    /// Default: `manual` in the TUI, `auto` for a non-interactive
    /// `ask --todo "..."`, where there is nobody to ask.
    #[arg(long, value_name = "manual|auto")]
    pub approve: Option<String>,

    /// Live proof for the controlled task lifecycle (task 15): `machine`
    /// (the whole transition table offline — every illegal transition must
    /// be refused *and* leave the run byte-identical), `gate` (live: the
    /// model is told to skip the plan and the run must stay on `plan`),
    /// `resume` (a run picked back up from disk, not from memory),
    /// `cleanup` (where the run lives on disk and when it is removed),
    /// `offline` or `all`. Exits after printing.
    #[arg(long, value_name = "WHICH")]
    pub verify_lifecycle: Option<String>,

    /// Connect to an MCP server over Streamable HTTP (initialize +
    /// notifications/initialized) and print the tools it exposes
    /// (`tools/list`). Without a URL, the public DeepWiki server is used.
    /// Exits after printing.
    #[arg(long, value_name = "URL", num_args = 0..=1, default_missing_value = mcp::DEFAULT_URL)]
    pub mcp_tools: Option<String>,

    /// Live proof for the task state machine: `machine` (legal transitions
    /// pass, illegal ones are refused — no network), `wire` (what the state
    /// puts into the system message — no network), `resume` (pause, then
    /// continue without re-explaining anything), `ladder` (the whole ladder
    /// on a task with a machine-checkable answer), `offline` or `all`.
    /// Exits after printing.
    #[arg(long, value_name = "WHICH")]
    pub verify_todo: Option<String>,

    /// Project invariants: `on` (default) or `off`.
    #[arg(long, value_name = "on|off", default_value = "on")]
    pub invariants: String,

    /// JSON file containing editable project invariants.
    #[arg(long, value_name = "FILE", env = "ASK_INVARIANTS_FILE")]
    pub inv_file: Option<String>,

    /// Offline causal proof: `wire`, `validate`, `retry`, `refuse`, or `all`.
    #[arg(long, value_name = "WHICH")]
    pub verify_invariants: Option<String>,

    /// Live proof for personalization: `wire` (what the profile puts into
    /// the system message — no network), `voice` (same question, two
    /// profiles, each answer carrying its own machine-checkable signature),
    /// `auto` (what the long-term `профиль.*` memory adds by itself) or
    /// `all`. Exits after printing.
    #[arg(long, value_name = "WHICH")]
    pub verify_profile: Option<String>,

    /// Where the memory layers live: `<dir>/short`, `<dir>/working`,
    /// `<dir>/long`. Default: the snapshot's own `memory/` next to the
    /// binary, else `~/.ask6/memory`. Same as ASK_MEMORY_DIR.
    #[arg(long, value_name = "DIR", env = "ASK_MEMORY_DIR")]
    pub memory_dir: Option<String>,

    /// Model to run --verify-context / --verify-compress against. Any id the
    /// money guard allows live (glm-5.3-flash, deepseek-flash, OpenRouter
    /// `:free`). Default: glm-5.3-flash.
    #[arg(long, value_name = "ID")]
    pub verify_model: Option<String>,

    /// Check live whether spend goes to the metered API or the GLM Coding
    /// Plan subscription, print the verdict and exit.
    #[arg(long, visible_alias = "billing")]
    pub verify_billing: bool,

    /// Run the live z.ai lever proof (glm-5.3-flash only) and exit.
    #[arg(long, visible_alias = "verify-stop")]
    pub verify: bool,

    /// Prove agent-box isolation: 100 boxes in one process, then a live
    /// secret-token recall across separate sessions. Exits after printing.
    #[arg(long, visible_alias = "verify-boxes")]
    pub verify_isolation: bool,

    /// With --verify-isolation, run only the checks that need no network.
    #[arg(long)]
    pub offline: bool,

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

    /// Print the model catalog grouped by provider, marking each id live,
    /// refused (paid), or hidden behind a missing key. Exits after printing.
    #[arg(long)]
    pub models: bool,

    /// With --models: live-list each connected provider's /models endpoint
    /// and diff it against the static catalog (drift report, no auto-edit).
    #[arg(long)]
    pub live: bool,

    /// Show every provider's key state: source (env/store), masked
    /// key, and the last live check. Exits after printing.
    #[arg(long)]
    pub keys: bool,

    /// Connect a provider's API key: hidden prompt (or --key-stdin), live
    /// check against the provider, then 0600 storage in ~/.ask6/auth.json.
    /// Value is a provider id (glm | deepseek | openrouter); bare --login
    /// opens an interactive picker. A rejected key is not saved (exit 1).
    #[arg(long, num_args = 0..=1, value_name = "PROVIDER")]
    pub login: Option<String>,

    /// With --login: read the key from stdin instead of the hidden prompt
    /// (for scripts and CI).
    #[arg(long)]
    pub key_stdin: bool,

    /// Remove a provider's key from the local store. An env var, if any,
    /// stays — unset it yourself.
    #[arg(long, value_name = "PROVIDER")]
    pub logout: Option<String>,

    /// Live-recheck every configured key against its provider and print the
    /// verdicts with evidence.
    #[arg(long)]
    pub verify_login: bool,
}

impl Cli {
    pub fn to_settings(&self) -> Res<Settings> {
        let mut s = Settings::default();
        if let Some(m) = &self.model {
            // Three-way: unknown id / known but keyless provider / usable.
            // The second branch is the only place a parse-time check reads
            // real key state; `config::find_model` + the provider list keep
            // the logic pure apart from that one call.
            match config::find_model(m) {
                None => return Err(config::catalog_error(m)),
                Some(cm) => {
                    let connected = auth::connected_providers();
                    if !connected.contains(&cm.provider) {
                        return Err(format!(
                            "model `{m}` needs a {} key: run `ask --login {}` (or set ${})",
                            cm.provider.label(),
                            cm.provider.id(),
                            cm.provider.env_var()
                        ));
                    }
                    s.model = m.clone();
                }
            }
        }
        if let Some(e) = &self.effort {
            s.effort = Effort::parse(e)?;
        }
        // Профиль проверяем по тому же каталогу, который увидит агент:
        // неизвестное имя должно падать здесь, а не молча уезжать в `off`.
        if let Some(p) = &self.profile {
            // Проверяем по тому же каталогу, который увидит агент, но файл
            // не трогаем: флаг действует на один запуск, а `active` в файле
            // — это то, что человек выбрал в TUI.
            s.profile = profile::ProfileSet::open(memory::memory_root()).resolve(p)?;
        }
        if self.todo {
            s.todo = true;
        }
        // Кто подписывает план. Без флага: в TUI ждём человека, а в
        // неинтерактивном заходе спросить некого — подпись ставит `auto`,
        // и это видно в журнале строкой `approved-by=auto`. Гейт при этом
        // проходится в обоих случаях.
        s.approve = match &self.approve {
            Some(v) => crate::run::ApprovePolicy::parse(v)?,
            None if self.todo && !self.question.is_empty() => crate::run::ApprovePolicy::Auto,
            None => crate::run::ApprovePolicy::Manual,
        };
        s.invariants = match self.invariants.as_str() {
            "on" => true,
            "off" => false,
            other => return Err(format!("--invariants: ожидалось on или off, получено `{other}`")),
        };
        if let Some(path) = &self.json_schema_file {
            let raw =
                std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
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
        if let Some(c) = &self.compress {
            s.context_strategy = config::ContextStrategy::parse(c)?;
        }
        if let Some(n) = self.keep_recent {
            if n == 0 {
                return Err("--keep-recent must be at least 1".into());
            }
            s.keep_recent = n;
        }
        if let Some(n) = self.summarize_every {
            if n == 0 {
                return Err("--summarize-every must be at least 1".into());
            }
            s.summarize_every = n;
        }
        if let Some(k) = self.top_k {
            if k == 0 || k < -1 {
                return Err(format!(
                    "top_k must be -1 (off) or a positive count (got {k})"
                ));
            }
            s.top_k = Some(k);
        }
        Ok(s)
    }
}

pub fn run() -> Res<()> {
    let cli = Cli::parse();

    if cli.keys {
        return print_keys();
    }
    if let Some(id) = cli.logout.as_deref() {
        return logout(id);
    }
    if cli.login.is_some() || cli.key_stdin {
        return login(cli.login.as_deref(), cli.key_stdin);
    }
    if cli.verify_login {
        return verify_login();
    }
    if cli.sessions {
        return print_sessions();
    }
    if cli.models {
        return print_models(cli.live);
    }
    if cli.live {
        return Err("--live is only meaningful together with --models".into());
    }

    // `--memory-dir` доезжает до памяти через ту же переменную, что и
    // `ASK_MEMORY_DIR`: у `memory_root()` остаётся один источник правды.
    if let Some(dir) = cli.memory_dir.as_deref().filter(|d| !d.is_empty()) {
        std::env::set_var("ASK_MEMORY_DIR", dir);
    }
    if let Some(path) = cli.inv_file.as_deref().filter(|p| !p.is_empty()) {
        std::env::set_var("ASK_INVARIANTS_FILE", path);
    }

    if cli.profiles {
        println!("{}", profile::ProfileSet::open(memory::memory_root()).listing());
        return Ok(());
    }

    let settings = cli.to_settings()?;

    if let Some(which) = cli.verify_invariants.as_deref() {
        println!("{}", crate::invariants::verify(which)?);
        return Ok(());
    }

    if cli.verify_billing {
        let report = billing::run()?;
        print!("{}", report.render());
        if !report.confirmed() {
            return Err("billing is not pay-per-token".into());
        }
        return Ok(());
    }

    if cli.verify {
        let report = verify::run()?;
        println!("{}", report.render());
        return Ok(());
    }

    if let Some(which) = cli.verify_context.as_deref() {
        let checks = verify::ContextCheck::parse(which)?;
        println!(
            "проверяю стратегии: {}",
            checks
                .iter()
                .map(|c| c.label())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let reports = verify::run_context(&checks, cli.verify_model.as_deref())?;
        let mut calls = 0;
        for report in &reports {
            print!("{}", report.render());
            println!();
            calls += report.calls();
        }
        for report in &reports {
            println!("{}", report.status_line());
        }
        println!("живых вызовов всего: {calls}");
        if let Some(bad) = reports.iter().find(|r| !r.confirmed()) {
            return Err(format!(
                "context strategy not confirmed: {}",
                bad.status_line()
            ));
        }
        return Ok(());
    }

    if let Some(which) = cli.verify_memory.as_deref() {
        let checks = verify::MemoryCheck::parse(which)?;
        println!(
            "проверяю модель памяти: {}",
            checks
                .iter()
                .map(|c| c.label())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let reports = verify::run_memory(&checks, cli.verify_model.as_deref(), cli.offline)?;
        let mut calls = 0;
        for report in &reports {
            print!("{}", report.render());
            println!();
            calls += report.calls();
        }
        for report in &reports {
            println!("{}", report.status_line());
        }
        println!("живых вызовов всего: {calls}");
        if let Some(bad) = reports.iter().find(|r| !r.confirmed()) {
            return Err(format!("memory model not confirmed: {}", bad.status_line()));
        }
        return Ok(());
    }

    if let Some(url) = cli.mcp_tools.as_deref() {
        println!("MCP: подключаюсь к {url}");
        let mut conn = mcp::Connection::connect(url)?;
        println!(
            "соединение установлено: {} {} (протокол {})",
            conn.server_name, conn.server_version, conn.protocol_version
        );
        let tools = conn.list_tools()?;
        println!("инструментов: {}", tools.len());
        for t in &tools {
            println!("\n• {}({})", t.name, t.params.join(", "));
            let first = t.description.lines().next().unwrap_or("");
            if !first.is_empty() {
                println!("  {first}");
            }
        }
        return Ok(());
    }

    if let Some(which) = cli.verify_lifecycle.as_deref() {
        let checks = verify::LifeCheck::parse(which)?;
        println!(
            "проверяю жизненный цикл задачи: {}",
            checks
                .iter()
                .map(|c| if c.offline() {
                    format!("{} (без сети)", c.label())
                } else {
                    c.label().to_string()
                })
                .collect::<Vec<_>>()
                .join(", ")
        );
        let reports = verify::run_lifecycle(&checks, cli.verify_model.as_deref())?;
        let mut calls = 0;
        for report in &reports {
            print!("{}", report.render());
            println!();
            calls += report.calls;
        }
        for report in &reports {
            println!("{}", report.status_line());
        }
        println!("живых вызовов всего: {calls}");
        if let Some(bad) = reports.iter().find(|r| !r.confirmed()) {
            return Err(format!("lifecycle not confirmed: {}", bad.status_line()));
        }
        return Ok(());
    }

    if let Some(which) = cli.verify_todo.as_deref() {
        let checks = verify::TodoCheck::parse(which)?;
        println!(
            "проверяю состояние задачи: {}",
            checks
                .iter()
                .map(|c| if c.offline() {
                    format!("{} (без сети)", c.label())
                } else {
                    c.label().to_string()
                })
                .collect::<Vec<_>>()
                .join(", ")
        );
        let reports = verify::run_todo(&checks, cli.verify_model.as_deref())?;
        let mut calls = 0;
        for report in &reports {
            print!("{}", report.render());
            println!();
            calls += report.calls();
        }
        for report in &reports {
            println!("{}", report.status_line());
        }
        println!("живых вызовов всего: {calls}");
        if let Some(bad) = reports.iter().find(|r| !r.confirmed()) {
            return Err(format!("todo not confirmed: {}", bad.status_line()));
        }
        return Ok(());
    }

    if let Some(which) = cli.verify_profile.as_deref() {
        let checks = verify::ProfileCheck::parse(which)?;
        println!(
            "проверяю персонализацию: {}",
            checks
                .iter()
                .map(|c| c.label())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let reports = verify::run_profile(&checks, cli.verify_model.as_deref())?;
        let mut calls = 0;
        for report in &reports {
            print!("{}", report.render());
            println!();
            calls += report.calls();
        }
        for report in &reports {
            println!("{}", report.status_line());
        }
        println!("живых вызовов всего: {calls}");
        if let Some(bad) = reports.iter().find(|r| !r.confirmed()) {
            return Err(format!("profile not confirmed: {}", bad.status_line()));
        }
        return Ok(());
    }

    if cli.verify_compress {
        let report = verify::run_compression(cli.verify_model.as_deref())?;
        print!("{}", report.render());
        if !report.confirmed() {
            return Err(format!(
                "compression not confirmed: {}",
                report.status_line()
            ));
        }
        return Ok(());
    }

    if cli.verify_isolation {
        let report = isolation::run(cli.offline)?;
        print!("{}", report.render());
        if !report.all_confirmed() {
            return Err("isolation not fully confirmed".into());
        }
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

    if settings.todo {
        // Тудушка включена — один вопрос превращается в лестницу этапов,
        // ровно как в TUI: по запросу на этап, переход только на
        // подтверждении этапа.
        return todo_shot(settings, &question);
    }
    one_shot(&cli, settings, &question, loaded)
}

/// Лестница этапов в один заход (`ask --todo "..."`). Печатает каждый
/// переход — то же «явно показывает, когда переходит на следующий шаг», но
/// в терминал, а не в панель.
///
/// Правила переходов сюда не копируются: и TUI, и этот путь двигают один и
/// тот же [`crate::run::TaskRun`] через одну и ту же таблицу. Разница
/// только в дефолте гейта — в неинтерактивном заходе плану некому сказать
/// «ок», поэтому подпись ставит `auto`, и это видно в журнале.
fn todo_shot(settings: Settings, question: &str) -> Res<()> {
    let mut agent = crate::agent::Agent::new(settings.clone())?;
    let mut run = crate::run::TaskRun::new("one-shot", &settings);
    run.apply(crate::run::Event::Start(question.to_string()))
        .map_err(|r| r.why)?;

    let mut history: Vec<api::ChatMessage> = Vec::new();
    loop {
        if run.awaiting_approval() {
            // Гейт проходится, а не пропускается: в одноразовом заходе
            // подпись ставит `auto`, но переход всё равно идёт событием
            // `Approve` и ложится в журнал.
            if settings.approve == crate::run::ApprovePolicy::Auto {
                run.apply(crate::run::Event::Approve("auto".into()))
                    .map_err(|r| r.why)?;
                eprintln!(
                    "\u{2714} план утверждён автоматически (approved-by=auto) \u{b7} {}",
                    run.line()
                );
            } else {
                eprintln!("{}", run.line());
                return Err(format!(
                    "план этапа `plan` закрыт и ждёт утверждения, а спросить некого: \
                     запустите с --approve auto или ведите задачу в TUI ({})",
                    run.log_tail(1)
                ));
            }
        }
        if !run.running() {
            break;
        }
        eprintln!("{}", run.state.enter_line());
        let set = agent.invariants().clone();
        let inv_on = agent.settings().invariants;
        let hist = history.clone();
        let mut sent_prompt = String::new();
        let outcome = crate::run::Engine::step(
            &mut run,
            &set,
            inv_on,
            None,
            |prompt, _, retry_note| {
                sent_prompt = prompt.to_string();
                let mut attempt = hist.clone();
                attempt.push(api::ChatMessage::user(prompt.to_string()));
                if let Some(note) = retry_note {
                    attempt.push(api::ChatMessage::user(note));
                }
                agent.complete(&attempt)
            },
            &mut |r: &crate::run::TaskRun| {
                // Одноразовый заход ничего не хранит на диске: прогон
                // живёт ровно столько, сколько идёт процесс. Фазу всё
                // равно показываем — по ней видно, где нас оборвёт.
                let _ = r;
            },
        )?;
        agent.set_todo(run.state.clone());
        match outcome {
            crate::run::StepOutcome::Advanced { closed, summary, next, text } => {
                eprintln!("\u{2192} следующий этап: `{}`", next.id());
                println!("{}", text.trim());
                println!();
                history.push(api::ChatMessage::user(sent_prompt));
                history.push(api::ChatMessage::assistant(text));
                eprintln!("{}", crate::todo::leave_line(closed, &summary));
            }
            crate::run::StepOutcome::AwaitingApproval { stage, summary, text } => {
                println!("{}", text.trim());
                println!();
                history.push(api::ChatMessage::user(sent_prompt));
                history.push(api::ChatMessage::assistant(text));
                eprintln!("{}", crate::todo::leave_line(stage, &summary));
                eprintln!(
                    "\u{270b} этап `{}` закрыт и ждёт утверждения \u{2014} до подписи реализация не начнётся",
                    stage.id()
                );
            }
            crate::run::StepOutcome::Done { pass, why, text } => {
                if let Some(t) = text {
                    println!("{}", t.trim());
                }
                eprintln!("{}", run.line());
                return if pass {
                    Ok(())
                } else {
                    Err(format!("прогон закрыт как done(fail): {why}"))
                };
            }
            crate::run::StepOutcome::Paused { why, text } => {
                if let Some(t) = text {
                    println!("{}", t.trim());
                }
                eprintln!("\u{23f8} {why}");
                return Err(format!(
                    "лестница не дошла до done: остановились на этапе `{}` ({why})",
                    run.state.stage.id()
                ));
            }
            crate::run::StepOutcome::Refused(r) => {
                eprintln!("{}", r.line());
                return Err(format!("переход отклонён: {}", r.why));
            }
        }
    }
    eprintln!("{}", run.line());
    if run.status != crate::run::RunStatus::DonePass {
        return Err(format!(
            "лестница не дошла до done: остановились на `{}` ({})",
            run.status.as_str(),
            run.state.stage.id()
        ));
    }
    Ok(())
}

/// The one-shot path runs through the multi-agent runtime rather than around
/// it: one `Runtime`, one `AgentBox`, one turn. The box is what enforces the
/// input/output policies and owns the session, so this path exercises the
/// same code a hundred concurrent boxes would.
fn one_shot(
    cli: &Cli,
    settings: Settings,
    question: &str,
    loaded: Option<session::Session>,
) -> Res<()> {
    let mut rt = Runtime::for_model(&settings.model, session::sessions_dir())?;
    let spec = BoxSpec::new("", settings.clone());
    let id = match loaded {
        Some(s) => rt.resume(&s.id, spec)?,
        None => rt.spawn(spec),
    };
    let turn = rt.get_mut(&id).ok_or("box vanished")?.ask(question)?;

    if let Some(why) = turn.refusal() {
        return Err(why);
    }
    let reply = turn.reply.as_ref().ok_or("no reply on an accepted turn")?;

    if cli.raw {
        println!(
            "{}",
            serde_json::to_string_pretty(&reply.raw).unwrap_or_default()
        );
    } else {
        let (display, parse_note) = if settings.json_mode.enabled {
            render::render_json_reply(&turn.text)
        } else {
            (turn.text.clone(), None)
        };
        if display.is_empty() {
            println!("(no content)");
        } else {
            println!("{display}");
        }
        if reply.truncated_by_max_chars {
            // Потолок мог приехать из профиля, а не из `--max-chars`:
            // печатаем тот, по которому реально резали.
            let cap = rt
                .get(&id)
                .and_then(|b| b.effective_settings().max_chars)
                .or(settings.max_chars)
                .unwrap_or(0);
            eprintln!("! truncated to max_chars={cap}");
        }
        if let Some(e) = parse_note {
            eprintln!("! {e}");
        }
    }

    if !cli.quiet {
        let u = &reply.usage;
        let b = rt.get(&id).ok_or("box vanished")?;
        eprintln!(
            "« model={}  finish={}  tokens: prompt={} completion={} (reasoning={}) total={}  ~${:.6}  {}ms  |  {}",
            if reply.model.is_empty() { "?" } else { &reply.model },
            reply.finish_reason.as_deref().unwrap_or("?"),
            u.prompt_tokens,
            u.completion_tokens,
            u.reasoning_tokens,
            u.total_tokens,
            billing::cost_usd(u),
            reply.latency_ms,
            b.settings().summary(),
        );
        for f in b.context_files() {
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
    if let Err(e) = rt.close(&id) {
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

// --- login flows ------------------------------------------------------------

/// `--keys`: one row per provider — source (env/store), masked key, last live check.
fn print_keys() -> Res<()> {
    let rows = auth::status_all();
    println!(
        "{:<12} {:<34} {:<24} last live check",
        "provider", "source (env/store)", "key"
    );
    for row in &rows {
        let (source, key) = match (&row.source, &row.masked) {
            (Some(s), Some(k)) => (s.describe().to_string(), k.clone()),
            _ => ("—".to_string(), "—".to_string()),
        };
        let check = match &row.last_check {
            Some(c) => format!(
                "{} {} ({})",
                c.verdict,
                session::format_updated(c.at),
                c.evidence
            ),
            None => "—".to_string(),
        };
        println!(
            "{:<12} {:<34} {:<24} {}",
            row.provider.id(),
            source,
            key,
            check
        );
    }
    if rows.iter().all(|r| !r.connected()) {
        println!(
            "\nno keys configured — run `ask --login <provider>` (stored in {}, never in the app)",
            auth::auth_path().display()
        );
    }
    Ok(())
}

/// `--models`: catalog grouped by provider. Offline unless `--live`.
fn print_models(live: bool) -> Res<()> {
    let connected = auth::connected_providers();
    for &p in &Provider::ALL {
        let has = connected.contains(&p);
        println!(
            "{} ({})  {}",
            p.label(),
            p.id(),
            if has { "connected" } else { "no key" }
        );
        for m in config::MODEL_CATALOG.iter().filter(|m| m.provider == p) {
            let tag = if !has {
                "no key"
            } else if api::is_live_model(m.provider, m.id) {
                "live"
            } else {
                "refused (paid)"
            };
            println!("  {:<52} {tag}", m.id);
        }
        println!();
    }
    let missing: Vec<&str> = Provider::ALL
        .iter()
        .filter(|p| !connected.contains(p))
        .map(|p| p.id())
        .collect();
    if !missing.is_empty() {
        println!(
            "unlock a provider: {}",
            missing
                .iter()
                .map(|id| format!("ask --login {id}"))
                .collect::<Vec<_>>()
                .join(" · ")
        );
    }
    if !live {
        return Ok(());
    }
    println!("--live: GET /models per connected provider vs static catalog");
    for &p in &connected {
        let ep = match Endpoint::for_provider(p) {
            Ok(ep) => ep,
            Err(e) => {
                println!("  {}: cannot resolve key ({e})", p.id());
                continue;
            }
        };
        match api::list_model_ids(&ep) {
            Ok(up) => {
                let catalog: Vec<&str> = config::MODEL_CATALOG
                    .iter()
                    .filter(|m| m.provider == p)
                    .map(|m| m.id)
                    .collect();
                let gone: Vec<&str> = catalog
                    .iter()
                    .copied()
                    .filter(|id| !up.iter().any(|u| u == id))
                    .collect();
                let new: Vec<&str> = up
                    .iter()
                    .map(|s| s.as_str())
                    .filter(|id| !catalog.contains(id))
                    .collect();
                println!(
                    "  {}: upstream {} id(s), catalog {}",
                    p.id(),
                    up.len(),
                    catalog.len()
                );
                for id in &gone {
                    println!("    in catalog but gone upstream: {id}");
                }
                for id in &new {
                    println!("    new upstream, not in catalog: {id}");
                }
                if gone.is_empty() && new.is_empty() {
                    println!("    no drift");
                }
            }
            Err(e) => println!("  {}: live list failed: {e}", p.id()),
        }
    }
    Ok(())
}

/// `--logout`: drop the key from the local store. An env var is out of reach.
fn logout(id: &str) -> Res<()> {
    let provider = Provider::parse(id)?;
    match auth::disconnect(provider) {
        Ok(true) => println!(
            "{}: key removed from {}",
            provider.label(),
            auth::auth_path().display()
        ),
        Ok(false) => println!(
            "{}: nothing stored in {} (an env var, if any, stays — unset ${} yourself)",
            provider.label(),
            auth::auth_path().display(),
            provider.env_var()
        ),
        Err(e) => return Err(e),
    }
    Ok(())
}

/// `--verify-login`: live-recheck everything configured.
fn verify_login() -> Res<()> {
    let mut configured = Vec::new();
    for &p in &Provider::ALL {
        match auth::resolve(p) {
            Ok(r) => configured.push((p, r.source.describe())),
            Err(_) => println!("{:<12} — not configured (ask --login {})", p.id(), p.id()),
        }
    }
    if configured.is_empty() {
        return Err(format!(
            "no keys configured — run `ask --login <provider>`; keys live in {}",
            auth::auth_path().display()
        ));
    }
    let mut rejected = 0;
    for (p, source) in configured {
        match auth::recheck(p) {
            Ok(CheckResult::Confirmed { evidence }) => {
                println!("{:<12} CONFIRMED    [{source}] {evidence}", p.id())
            }
            Ok(CheckResult::Rejected { http, msg }) => {
                rejected += 1;
                println!("{:<12} REJECTED     [{source}] HTTP {http}: {msg}", p.id())
            }
            Ok(CheckResult::Unreachable { msg }) => {
                println!("{:<12} UNREACHABLE  [{source}] {msg}", p.id())
            }
            Err(e) => println!("{:<12} ERROR        {e}", p.id()),
        }
    }
    if rejected > 0 {
        return Err(format!(
            "{rejected} configured key(s) were rejected by their provider"
        ));
    }
    Ok(())
}

/// `--login`: hidden prompt (or stdin), live check, 0600 store.
fn login(provider_arg: Option<&str>, key_stdin: bool) -> Res<()> {
    let provider = match provider_arg.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Provider::parse(s)?,
        None if key_stdin => {
            return Err("--key-stdin needs a provider: ask --login glm --key-stdin".into())
        }
        None => pick_provider()?,
    };
    let key = if key_stdin {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("cannot read stdin: {e}"))?;
        buf.trim().to_string()
    } else {
        prompt_key(provider)?
    };
    if key.is_empty() {
        return Err("empty key — nothing connected".into());
    }
    println!("checking {} key live…", provider.label());
    match auth::connect(provider, &key)? {
        (CheckResult::Confirmed { evidence }, true) => {
            println!("CONFIRMED — {} key works: {evidence}", provider.label());
            println!(
                "saved to {} (mode 0600, never committed)",
                auth::auth_path().display()
            );
        }
        (CheckResult::Unreachable { msg }, true) => {
            println!(
                "{}: provider unreachable ({msg}) — key saved UNVERIFIED; run `ask --verify-login` later",
                provider.label()
            );
        }
        (CheckResult::Rejected { http, msg }, false) => {
            return Err(format!("REJECTED (HTTP {http}): {msg} — key NOT saved"));
        }
        _ => unreachable!("connect returns only the verdicts above"),
    }
    Ok(())
}

/// Bare `--login` with a terminal: numbered provider picker.
fn pick_provider() -> Res<Provider> {
    if !std::io::stdin().is_terminal() {
        return Err("specify a provider: ask --login glm|deepseek|openrouter".into());
    }
    println!("Which provider do you want to connect?");
    for (i, p) in Provider::ALL.iter().enumerate() {
        println!("  {}. {} ({})", i + 1, p.label(), p.id());
    }
    print!("1-3: ");
    use std::io::Write;
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("cannot read choice: {e}"))?;
    let idx: usize = line
        .trim()
        .parse()
        .map_err(|_| format!("not a number: {}", line.trim()))?;
    Provider::ALL
        .get(idx.wrapping_sub(1))
        .copied()
        .ok_or_else(|| format!("no provider #{idx} (1-3)"))
}

/// Reads an API key without echo: raw mode, one char at a time, `*` shown
/// per char. Esc cancels; Ctrl-C restores the terminal and cancels. No new
/// dependencies — plain crossterm, which the TUI already pulls in.
fn prompt_key(provider: Provider) -> Res<String> {
    use crossterm::event::{read, Event, KeyCode, KeyEventKind, KeyModifiers};
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

    if !std::io::stdin().is_terminal() {
        return Err("no terminal for a hidden prompt — use --key-stdin".into());
    }
    println!(
        "{} API key (input hidden; Enter submits, Esc cancels):",
        provider.label()
    );
    enable_raw_mode().map_err(|e| format!("cannot enable raw mode: {e}"))?;
    let mut key = String::new();
    let outcome = loop {
        match read() {
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => match k.code {
                KeyCode::Enter => break Ok(key),
                KeyCode::Esc => break Err("login cancelled".into()),
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    break Err("login cancelled (Ctrl-C)".into());
                }
                KeyCode::Backspace => {
                    key.pop();
                }
                KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                    key.push(c);
                    print!("*");
                    use std::io::Write;
                    std::io::stdout().flush().ok();
                }
                _ => {}
            },
            Ok(_) => {}
            Err(e) => break Err(format!("cannot read key input: {e}")),
        }
    };
    disable_raw_mode().map_err(|e| format!("cannot restore terminal: {e}"))?;
    println!();
    outcome.map(|k| k.trim().to_string())
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
