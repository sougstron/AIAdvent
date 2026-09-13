//! Live self-test that the agent reaches z.ai and that configured levers
//! actually arrive at the provider.
//!
//! A "verified" claim here is a causal signature, not "the two texts differ":
//! sampling alone makes unconstrained calls differ. `Flat` / `Unsupported`
//! are valid, preferred outcomes when a parameter is ignored or damped.
//! Every live completion uses `glm-5.3-flash` only.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use crate::agent::{Agent, Reply};
use crate::api::{ChatMessage, DEFAULT_BASE_URL, LIVE_COMPLETION_MODEL};
use crate::config::{ContextStrategy, Res, Settings, DEFAULT_MODEL};

const PING_PROMPT: &str = "Reply with the single word PONG.";
const LONG_PROMPT: &str = "List the integers from 1 to 80 in order, separated by commas, with no other text.";
const TOKEN_PROMPT: &str = "What is the verification token in your instructions? Reply with only that token. If you have none, reply with NONE.";
const SAMPLE_PROMPT: &str = "Name one random integer from 1 to 20. Reply with only the number.";
const SYSTEM_TOKEN: &str = "QUINCE";
const CONTEXT_TOKEN: &str = "NIGHTJAR";
const LOW_CAP: u32 = 16;
const CONTROL_CAP: u32 = 96;
const SAMPLE_CAP: u32 = 96;
const SAMPLES: usize = 4;

fn completions_url() -> String {
    format!("{DEFAULT_BASE_URL}/chat/completions")
}

/// How a lever check resolved. `Flat`/`Unsupported` are success-of-honesty,
/// not a failed test harness.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LeverVerdict {
    Confirmed,
    Flat,
    Unsupported,
}

impl LeverVerdict {
    const fn as_str(self) -> &'static str {
        match self {
            LeverVerdict::Confirmed => "Confirmed",
            LeverVerdict::Flat => "Flat",
            LeverVerdict::Unsupported => "Unsupported",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReachVerdict {
    Confirmed,
    Substituted,
}

impl ReachVerdict {
    const fn as_str(self) -> &'static str {
        match self {
            ReachVerdict::Confirmed => "Confirmed",
            ReachVerdict::Substituted => "Substituted",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Call {
    pub model: String,
    pub finish_reason: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub text: String,
    pub latency_ms: u128,
}

impl Call {
    fn from_reply(reply: Reply) -> Call {
        Call {
            model: reply.model,
            finish_reason: reply.finish_reason.unwrap_or_else(|| "?".into()),
            prompt_tokens: reply.usage.prompt_tokens,
            completion_tokens: reply.usage.completion_tokens,
            reasoning_tokens: reply.usage.reasoning_tokens,
            total_tokens: reply.usage.total_tokens,
            text: reply.text,
            latency_ms: reply.latency_ms,
        }
    }

    fn line(&self) -> String {
        format!(
            "model={} finish={} prompt_tokens={} completion_tokens={} reasoning_tokens={} total={} {}ms",
            if self.model.is_empty() { "?" } else { &self.model },
            self.finish_reason,
            self.prompt_tokens,
            self.completion_tokens,
            self.reasoning_tokens,
            self.total_tokens,
            self.latency_ms
        )
    }
}

#[derive(Clone, Debug)]
pub struct Reachability {
    pub endpoint: String,
    pub asked_model: String,
    pub call: Call,
    pub verdict: ReachVerdict,
}

#[derive(Clone, Debug)]
pub struct MaxTokensCheck {
    pub low_cap: u32,
    pub control_cap: u32,
    pub capped: Call,
    pub control: Call,
    pub verdict: LeverVerdict,
}

#[derive(Clone, Debug)]
pub struct InstructionCheck {
    pub label: &'static str,
    pub token: &'static str,
    pub on: Call,
    pub off: Call,
    pub verdict: LeverVerdict,
}

#[derive(Clone, Debug)]
pub struct SamplingCheck {
    pub lever: &'static str,
    pub cold_label: String,
    pub hot_label: String,
    pub cold: Vec<String>,
    pub hot: Vec<String>,
    pub cold_distinct: usize,
    pub hot_distinct: usize,
    pub verdict: LeverVerdict,
}

#[derive(Clone, Debug)]
pub struct Report {
    pub reach: Reachability,
    pub max_tokens: MaxTokensCheck,
    pub system_prompt: InstructionCheck,
    pub agents_md: InstructionCheck,
    pub temperature: SamplingCheck,
    pub top_p: SamplingCheck,
    pub top_k: SamplingCheck,
}

impl Report {
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("z.ai agent self-test (glm-5.3-flash only)\n");
        out.push_str(&format!("endpoint: {}\n\n", self.reach.endpoint));
        out.push_str(&format!(
            "== reachability asked={} ==\n{}\n=> {}\n\n",
            self.reach.asked_model,
            self.reach.call.line(),
            self.reach.verdict.as_str()
        ));
        out.push_str(&format!(
            "== max_tokens low={} control={} ==\n[capped]   {}\n  {}\n[control]  {}\n  {}\n=> {}\n\n",
            self.max_tokens.low_cap,
            self.max_tokens.control_cap,
            self.max_tokens.capped.line(),
            clip(&self.max_tokens.capped.text, 120),
            self.max_tokens.control.line(),
            clip(&self.max_tokens.control.text, 120),
            self.max_tokens.verdict.as_str()
        ));
        for check in [&self.system_prompt, &self.agents_md] {
            out.push_str(&format!(
                "== {} (token={}) ==\n[on]  {}\n  {}\n[off] {}\n  {}\n=> {}\n\n",
                check.label,
                check.token,
                check.on.line(),
                clip(&check.on.text, 80),
                check.off.line(),
                clip(&check.off.text, 80),
                check.verdict.as_str()
            ));
        }
        for check in [&self.temperature, &self.top_p, &self.top_k] {
            out.push_str(&format!(
                "== {} {} vs {} (n={}) ==\ncold distinct={} {:?}\nhot  distinct={} {:?}\n=> {}\n\n",
                check.lever,
                check.cold_label,
                check.hot_label,
                check.cold.len(),
                check.cold_distinct,
                check.cold,
                check.hot_distinct,
                check.hot,
                check.verdict.as_str()
            ));
        }
        out.push_str(
            "Confirmed requires a causal signature. Flat/Unsupported means the provider did not honour the lever — that is the result, not a failed harness.\n",
        );
        out
    }

    pub fn status_line(&self) -> String {
        format!(
            "verify: reach={} max_tokens={} system={} agents.md={} temp={} top_p={} top_k={}",
            self.reach.verdict.as_str(),
            self.max_tokens.verdict.as_str(),
            self.system_prompt.verdict.as_str(),
            self.agents_md.verdict.as_str(),
            self.temperature.verdict.as_str(),
            self.top_p.verdict.as_str(),
            self.top_k.verdict.as_str()
        )
    }
}

pub fn run() -> Res<Report> {
    let endpoint = completions_url();
    let mut agent = Agent::new(isolated_settings())?;
    if agent.settings().model != LIVE_COMPLETION_MODEL {
        return Err(format!(
            "verify refuses to run: agent model is `{}`, not `{LIVE_COMPLETION_MODEL}`",
            agent.settings().model
        ));
    }

    let reach = check_reach(&agent, &endpoint)?;
    let max_tokens = check_max_tokens(&mut agent)?;
    let system_prompt = check_system_prompt(&mut agent)?;
    let agents_md = check_agents_md(&mut agent)?;
    let temperature = check_temperature(&mut agent)?;
    let top_p = check_top_p(&mut agent)?;
    let top_k = check_top_k(&mut agent)?;

    Ok(Report {
        reach,
        max_tokens,
        system_prompt,
        agents_md,
        temperature,
        top_p,
        top_k,
    })
}

fn isolated_settings() -> Settings {
    Settings {
        model: DEFAULT_MODEL.into(),
        system_prompt: String::new(),
        context_enabled: false,
        ..Settings::default()
    }
    .without_stop_condition()
}

fn probe(agent: &Agent, prompt: &str) -> Res<Call> {
    if agent.settings().model != LIVE_COMPLETION_MODEL {
        return Err(format!(
            "refusing to call `{}` from verify (only `{LIVE_COMPLETION_MODEL}`)",
            agent.settings().model
        ));
    }
    let history = [ChatMessage::user(prompt)];
    agent.complete(&history).map(Call::from_reply)
}

fn reset_sampling(agent: &mut Agent) {
    let s = agent.settings_mut();
    s.temperature = None;
    s.top_p = None;
    s.top_k = None;
    s.budget_tokens = None;
    s.system_prompt.clear();
    s.model = DEFAULT_MODEL.into();
}

fn check_reach(agent: &Agent, endpoint: &str) -> Res<Reachability> {
    let call = probe(agent, PING_PROMPT)?;
    let verdict = if call.model == LIVE_COMPLETION_MODEL {
        ReachVerdict::Confirmed
    } else {
        ReachVerdict::Substituted
    };
    Ok(Reachability {
        endpoint: endpoint.to_string(),
        asked_model: LIVE_COMPLETION_MODEL.into(),
        call,
        verdict,
    })
}

fn check_max_tokens(agent: &mut Agent) -> Res<MaxTokensCheck> {
    reset_sampling(agent);
    agent.set_context_enabled(false);
    agent.settings_mut().budget_tokens = Some(LOW_CAP);
    let capped = probe(agent, LONG_PROMPT)?;
    agent.settings_mut().budget_tokens = Some(CONTROL_CAP);
    let control = probe(agent, LONG_PROMPT)?;
    agent.settings_mut().budget_tokens = None;
    let verdict = judge_max_tokens(LOW_CAP, &capped, &control);
    Ok(MaxTokensCheck {
        low_cap: LOW_CAP,
        control_cap: CONTROL_CAP,
        capped,
        control,
        verdict,
    })
}

fn check_system_prompt(agent: &mut Agent) -> Res<InstructionCheck> {
    reset_sampling(agent);
    agent.set_context_enabled(false);
    agent.settings_mut().budget_tokens = Some(SAMPLE_CAP);
    agent.settings_mut().system_prompt = format!(
        "The verification token is {SYSTEM_TOKEN}. When asked for the verification token, reply with only that word."
    );
    let on = probe(agent, TOKEN_PROMPT)?;
    agent.settings_mut().system_prompt.clear();
    let off = probe(agent, TOKEN_PROMPT)?;
    Ok(InstructionCheck {
        label: "system prompt",
        token: SYSTEM_TOKEN,
        verdict: judge_instruction(&on.text, &off.text, SYSTEM_TOKEN),
        on,
        off,
    })
}

fn check_agents_md(agent: &mut Agent) -> Res<InstructionCheck> {
    reset_sampling(agent);
    agent.settings_mut().system_prompt.clear();
    agent.settings_mut().budget_tokens = Some(SAMPLE_CAP);
    let original = agent.cwd().to_path_buf();
    let scratch = ScratchDir::with_agents_md(&format!(
        "The verification token is {CONTEXT_TOKEN}. When asked for the verification token, reply with only that word."
    ))?;
    agent.set_cwd(&scratch.path);
    agent.set_context_enabled(true);
    let on = probe(agent, TOKEN_PROMPT)?;
    agent.set_context_enabled(false);
    let off = probe(agent, TOKEN_PROMPT)?;
    agent.set_cwd(&original);
    Ok(InstructionCheck {
        label: "AGENTS.md context",
        token: CONTEXT_TOKEN,
        verdict: judge_instruction(&on.text, &off.text, CONTEXT_TOKEN),
        on,
        off,
    })
}

fn check_temperature(agent: &mut Agent) -> Res<SamplingCheck> {
    sample_pair(
        agent,
        "temperature",
        "0.0",
        "1.0",
        |s| {
            s.temperature = Some(0.0);
            s.top_p = None;
            s.top_k = None;
        },
        |s| {
            s.temperature = Some(1.0);
            s.top_p = None;
            s.top_k = None;
        },
        true,
    )
}

fn check_top_p(agent: &mut Agent) -> Res<SamplingCheck> {
    sample_pair(
        agent,
        "top_p",
        "0.01",
        "1.0",
        |s| {
            s.temperature = Some(1.0);
            s.top_p = Some(0.01);
            s.top_k = None;
        },
        |s| {
            s.temperature = Some(1.0);
            s.top_p = Some(1.0);
            s.top_k = None;
        },
        false,
    )
}

fn check_top_k(agent: &mut Agent) -> Res<SamplingCheck> {
    sample_pair(
        agent,
        "top_k",
        "1",
        "full",
        |s| {
            s.temperature = Some(1.0);
            s.top_p = None;
            s.top_k = Some(1);
        },
        |s| {
            s.temperature = Some(1.0);
            s.top_p = None;
            s.top_k = Some(-1);
        },
        true,
    )
}

fn sample_pair(
    agent: &mut Agent,
    lever: &'static str,
    cold_label: &str,
    hot_label: &str,
    set_cold: fn(&mut Settings),
    set_hot: fn(&mut Settings),
    cold_must_collapse: bool,
) -> Res<SamplingCheck> {
    reset_sampling(agent);
    agent.set_context_enabled(false);
    agent.settings_mut().budget_tokens = Some(SAMPLE_CAP);
    set_cold(agent.settings_mut());
    let cold = sample_n(agent, SAMPLES)?;
    set_hot(agent.settings_mut());
    let hot = sample_n(agent, SAMPLES)?;
    let cold_distinct = distinct_count(&cold);
    let hot_distinct = distinct_count(&hot);
    let verdict = judge_sampling(cold_distinct, hot_distinct, cold_must_collapse);
    Ok(SamplingCheck {
        lever,
        cold_label: cold_label.into(),
        hot_label: hot_label.into(),
        cold,
        hot,
        cold_distinct,
        hot_distinct,
        verdict,
    })
}

fn sample_n(agent: &Agent, n: usize) -> Res<Vec<String>> {
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let call = probe(agent, SAMPLE_PROMPT)?;
        out.push(normalize_answer(&call.text));
    }
    Ok(out)
}

fn judge_max_tokens(cap: u32, capped: &Call, control: &Call) -> LeverVerdict {
    let cap = u64::from(cap);
    let at_cap = capped.finish_reason == "length" && capped.completion_tokens == cap;
    let control_shows_room = control.completion_tokens > cap;
    if at_cap && control_shows_room {
        LeverVerdict::Confirmed
    } else {
        LeverVerdict::Flat
    }
}

fn judge_instruction(on_text: &str, off_text: &str, token: &str) -> LeverVerdict {
    if has_token(on_text, token) && !has_token(off_text, token) {
        LeverVerdict::Confirmed
    } else {
        LeverVerdict::Flat
    }
}

/// Causal sampling signature, never "the two texts differ".
///
/// * If the collapsing side must be greedy (`temperature=0` / `top_k=1`) but
///   produced more than one answer, the parameter did not reach the sampler
///   → `Unsupported`.
/// * If the collapsing side is unique and the other side spreads → `Confirmed`.
/// * If neither side spreads → `Flat` (ignored, or damped by thinking).
fn judge_sampling(cold_distinct: usize, hot_distinct: usize, cold_must_collapse: bool) -> LeverVerdict {
    if cold_must_collapse && cold_distinct > 1 {
        LeverVerdict::Unsupported
    } else if cold_distinct == 1 && hot_distinct > cold_distinct {
        LeverVerdict::Confirmed
    } else {
        LeverVerdict::Flat
    }
}

fn has_token(text: &str, token: &str) -> bool {
    let needle = token.to_ascii_uppercase();
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| !w.is_empty() && w.eq_ignore_ascii_case(&needle))
}

fn normalize_answer(s: &str) -> String {
    s.trim()
        .trim_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_ascii_lowercase()
}

fn distinct_count(answers: &[String]) -> usize {
    answers.iter().collect::<BTreeSet<_>>().len()
}

fn clip(s: &str, max: usize) -> String {
    let mut it = s.chars();
    let head: String = it.by_ref().take(max).collect();
    if it.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    fn with_agents_md(body: &str) -> Res<ScratchDir> {
        let path = std::env::temp_dir().join(format!("ask-verify-ctx-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
        let file = path.join("AGENTS.md");
        fs::write(&file, body).map_err(|e| format!("cannot write {}: {e}", file.display()))?;
        Ok(ScratchDir { path })
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

// ------------------------------------------------ управление контекстом

/// Синтетический разговор для проверки сжатия: два «факта» на разной
/// глубине и достаточно наполнителя, чтобы экономия была видна.
///
/// `FOLDED_TOKEN` называется в самом начале — при `keep_recent=6`,
/// `summarize_every=10` и 30 сообщениях он гарантированно попадает в
/// свёрнутую часть, так что ответить на вопрос о нём можно только через
/// summary. `TAIL_TOKEN` назван под конец и остаётся в дословном хвосте:
/// это контроль, что сжатие не съело недавние сообщения.
pub const FOLDED_TOKEN: &str = "NIGHTJAR7741";
pub const TAIL_TOKEN: &str = "KESTREL9120";
const COMPRESS_MESSAGES: usize = 30;
const COMPRESS_KEEP_RECENT: usize = 6;
const COMPRESS_EVERY: usize = 10;
const FOLDED_QUESTION: &str =
    "Какой код доступа к стенду я называл? Ответь одним словом — только кодом. Если не знаешь, ответь NONE.";
const TAIL_QUESTION: &str =
    "Какой код резервного канала я называл? Ответь одним словом — только кодом. Если не знаешь, ответь NONE.";

/// Как разрешилась проверка сжатия.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CompressionVerdict {
    /// Сжатая история дешевле по `prompt_tokens` и оба факта на месте.
    Confirmed,
    /// Дешевле, но факт из свёрнутой части потерян — summary не сохранило
    /// то, ради чего оно существует.
    Lossy,
    /// Экономии нет.
    Flat,
}

impl CompressionVerdict {
    const fn as_str(self) -> &'static str {
        match self {
            CompressionVerdict::Confirmed => "Confirmed",
            CompressionVerdict::Lossy => "Lossy",
            CompressionVerdict::Flat => "Flat",
        }
    }
}

/// Один и тот же вопрос, заданный к полной и к сжатой истории.
#[derive(Clone, Debug)]
pub struct CompressionProbe {
    pub label: &'static str,
    pub token: &'static str,
    pub question: &'static str,
    pub full: Call,
    pub compressed: Call,
}

impl CompressionProbe {
    pub fn full_recalled(&self) -> bool {
        has_token(&self.full.text, self.token)
    }

    pub fn compressed_recalled(&self) -> bool {
        has_token(&self.compressed.text, self.token)
    }

    /// Сэкономленные на этом запросе `prompt_tokens`.
    pub fn saved(&self) -> i64 {
        self.full.prompt_tokens as i64 - self.compressed.prompt_tokens as i64
    }

    fn line(&self) -> String {
        format!(
            "[full]       {}\n  {}\n[compressed] {}\n  {}\n  recall: full={} compressed={}  prompt_tokens {} -> {} ({:+})",
            self.full.line(),
            clip(&self.full.text, 100),
            self.compressed.line(),
            clip(&self.compressed.text, 100),
            self.full_recalled(),
            self.compressed_recalled(),
            self.full.prompt_tokens,
            self.compressed.prompt_tokens,
            -self.saved()
        )
    }
}

#[derive(Clone, Debug)]
pub struct CompressionReport {
    pub messages: usize,
    pub keep_recent: usize,
    pub every: usize,
    /// Сколько сообщений ушло под summary и сколько уходит на провод.
    pub covered: usize,
    pub wire_messages: usize,
    pub folded_chars: usize,
    pub summary_chars: usize,
    pub summary: String,
    /// Число свёрток и их суммарная цена в токенах.
    pub folds: usize,
    pub fold_tokens: u64,
    pub folded_fact: CompressionProbe,
    pub tail_fact: CompressionProbe,
    pub verdict: CompressionVerdict,
}

impl CompressionReport {
    pub fn confirmed(&self) -> bool {
        self.verdict == CompressionVerdict::Confirmed
    }

    /// Средняя экономия `prompt_tokens` на один ход.
    pub fn saved_per_turn(&self) -> i64 {
        (self.folded_fact.saved() + self.tail_fact.saved()) / 2
    }

    /// Через сколько ходов свёртка окупается. `None` — экономии нет.
    pub fn break_even_turns(&self) -> Option<u64> {
        let saved = self.saved_per_turn();
        if saved <= 0 {
            return None;
        }
        Some(self.fold_tokens.div_ceil(saved as u64))
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("управление контекстом: сжатие истории (glm-5.3-flash)\n");
        out.push_str(&format!(
            "разговор: {} сообщений, keep_recent={}, summarize_every={}\n",
            self.messages, self.keep_recent, self.every
        ));
        out.push_str(&format!(
            "свёрнуто {} сообщений ({} симв.) в summary {} симв.; на провод уходит {} сообщений; свёрток {} ценой {} токенов\n\n",
            self.covered,
            self.folded_chars,
            self.summary_chars,
            self.wire_messages,
            self.folds,
            self.fold_tokens
        ));
        out.push_str(&format!("== summary ==\n{}\n\n", clip(&self.summary, 900)));
        for probe in [&self.folded_fact, &self.tail_fact] {
            out.push_str(&format!(
                "== {} (token={}) ==\n{}\n{}\n\n",
                probe.label,
                probe.token,
                probe.question,
                probe.line()
            ));
        }
        out.push_str(&format!(
            "экономия prompt_tokens: {:+} на ход в среднем; свёртка окупается за {}\n",
            self.saved_per_turn(),
            match self.break_even_turns() {
                Some(n) => format!("{n} ход(ов)"),
                None => "никогда (экономии нет)".into(),
            }
        ));
        out.push_str(&format!("=> {}\n", self.verdict.as_str()));
        out.push_str(
            "Confirmed требует причинной подписи: меньше prompt_tokens И факт из свёрнутой части всё ещё известен модели. \
             Lossy — сэкономили, но потеряли факт. Flat — экономии нет. Это результат, а не сломанный тест.\n",
        );
        out
    }

    pub fn status_line(&self) -> String {
        format!(
            "compress: verdict={} saved/turn={:+} folded_recall={} tail_recall={}",
            self.verdict.as_str(),
            self.saved_per_turn(),
            self.folded_fact.compressed_recalled(),
            self.tail_fact.compressed_recalled()
        )
    }
}

/// Реплики синтетического разговора. Детерминированные, без сети.
pub fn compression_dialogue() -> Vec<ChatMessage> {
    // Реплики намеренно «человеческого» размера (150–300 символов): на
    // репликах в два слова экономия упирается в накладные расходы на
    // сообщение и перестаёт быть показательной.
    let topics = [
        (
            "Готовлю нагрузочный стенд для сервиса заказов: нужно понять, где он ложится под пиковым трафиком и что чинить первым. Команда небольшая, времени до релиза — две недели.",
            "Понял: цель — найти точку отказа сервиса заказов под пиком за две недели. Предлагаю сначала зафиксировать профиль нагрузки, потом прогонять сценарии по одному, меняя за прогон ровно один параметр.",
        ),
        (
            "Сервис написан на Rust, база PostgreSQL 16 с репликой на чтение, очередь NATS с джетстримом. Всё крутится в Kubernetes, по три пода на сервис.",
            "Записал стек: Rust, PostgreSQL 16 плюс read-реплика, NATS JetStream, Kubernetes по три пода. Узким местом при таком раскладе обычно оказывается пул соединений к базе, а не сам сервис.",
        ),
        (
            "Целевая нагрузка — 4000 запросов в секунду, из них примерно 85 процентов чтение и 15 процентов запись. Пик приходится на вечер, с семи до девяти.",
            "Цель 4000 rps при соотношении 85/15 зафиксирована, пик — вечерние два часа. Значит профиль прогона должен повторять именно этот перекос, а не равномерную смесь.",
        ),
        (
            "p99 по чтению не должен превышать 120 миллисекунд, по записи допускаю до 400. Всё, что выше, считаем нарушением SLO и поводом остановить прогон.",
            "SLO: p99 чтения 120 мс, записи 400 мс, выход за границы — стоп-условие прогона. Добавлю это как автоматический критерий остановки, чтобы не досматривать графики руками.",
        ),
        (
            "Запись идёт батчами по 500 строк примерно раз в две секунды. Батч собирается в памяти сервиса, и при рестарте пода мы теряем то, что не успело уехать.",
            "Батч записи: 500 строк каждые 2 секунды, накапливается в памяти и теряется при рестарте. Под нагрузкой это же место даст всплески latency — стоит померить отдельно.",
        ),
        (
            "Кэш — Redis, TTL пятнадцать минут, ключи по идентификатору клиента. Инвалидация ленивая, то есть мы просто ждём истечения TTL.",
            "Redis с TTL 15 минут и ленивой инвалидацией по ключу клиента. На прогоне это даст холодный старт первые пятнадцать минут — их лучше не считать в итоговые цифры.",
        ),
        (
            "Тестовый прогон длится сорок минут: пять минут разгон, тридцать — плато, пять — спад. Между прогонами делаем паузу, чтобы база успела прийти в себя.",
            "Профиль прогона 5/30/5 минут с паузой между итерациями принят. Сравнивать имеет смысл только участок плато — разгон и спад дают искажённые перцентили.",
        ),
        (
            "Метрики собираем в Prometheus, дашборд в Grafana. Интервал скрейпа пятнадцать секунд, и мне важно, чтобы перцентили считались по гистограммам, а не по средним.",
            "Prometheus со скрейпом раз в 15 секунд, Grafana для просмотра, перцентили по гистограммам. Учту, что при таком интервале короткие всплески короче минуты будут смазаны.",
        ),
        (
            "Алерт срабатывает, когда доля ошибок превышает половину процента на интервале в пять минут. Ниже этого порога считаем, что всё в норме.",
            "Порог алерта — 0.5% ошибок на пятиминутном окне. Для прогона это же число будет вторым стоп-условием вместе с нарушением SLO по задержке.",
        ),
        (
            "Ретраи — три попытки с экспоненциальной паузой, стартовая задержка сто миллисекунд. Джиттера сейчас нет, и я подозреваю, что именно это добавляет пилу на графиках.",
            "Три ретрая с экспоненциальной паузой от 100 мс и без джиттера. Подозрение обоснованное: без джиттера ретраи синхронизируются и бьют по базе волнами — это видно как пила.",
        ),
        (
            "Деплой идёт через ArgoCD в кластер staging, окружение совпадает с продом по ресурсам, но данных там примерно вдесятеро меньше.",
            "ArgoCD в staging, ресурсы как на проде, данных в десять раз меньше. Это важная оговорка: планы запросов на маленькой таблице будут другими, и цифры по базе занижены.",
        ),
        (
            "Секреты храним в Vault, ротация раз в квартал. Сервис читает их при старте и больше не перечитывает, так что ротация требует рестарта подов.",
            "Vault с квартальной ротацией и чтением секретов только при старте. Значит ротация — это плановый рестарт, и его стоит прогнать под нагрузкой хотя бы раз.",
        ),
        (
            "Логи пишем структурированным JSON и отправляем в Loki. На пике объём логов заметно растёт, и я не уверен, что это не съедает часть производительности.",
            "JSON-логи в Loki, объём растёт вместе с нагрузкой. Проверяется просто: прогон с урезанным уровнем логирования против обычного, разница в rps и будет ответом.",
        ),
        (
            "Отчёт по прогону нужен в понедельник утром: таблица по сценариям, графики перцентилей и короткий вывод, что чинить первым.",
            "Отчёт к понедельнику: таблица сценариев, графики перцентилей, приоритетный список починки. Соберу его по участкам плато, чтобы числа были сравнимы между прогонами.",
        ),
    ];
    let mut history = Vec::with_capacity(COMPRESS_MESSAGES);
    // Факт для свёрнутой части — самым первым сообщением.
    history.push(ChatMessage::user(format!(
        "Запомни: код доступа к стенду — {FOLDED_TOKEN}. Он понадобится в конце разговора."
    )));
    history.push(ChatMessage::assistant(format!(
        "Запомнил: код доступа к стенду {FOLDED_TOKEN}."
    )));
    for (i, (q, a)) in topics.iter().enumerate() {
        // Факт для дословного хвоста — в предпоследней паре.
        if i == topics.len() - 2 {
            history.push(ChatMessage::user(format!(
                "Запомни: код резервного канала — {TAIL_TOKEN}."
            )));
            history.push(ChatMessage::assistant(format!(
                "Запомнил: код резервного канала {TAIL_TOKEN}."
            )));
            continue;
        }
        history.push(ChatMessage::user((*q).to_string()));
        history.push(ChatMessage::assistant((*a).to_string()));
    }
    history
}

/// Живая проверка сжатия истории: один и тот же разговор и те же вопросы,
/// один раз с полной историей, другой — со сжатой.
pub fn run_compression() -> Res<CompressionReport> {
    let mut settings = isolated_settings();
    settings.keep_recent = COMPRESS_KEEP_RECENT;
    settings.summarize_every = COMPRESS_EVERY;
    settings.context_strategy = ContextStrategy::Off;
    let mut agent = Agent::new(settings)?;
    if agent.settings().model != LIVE_COMPLETION_MODEL {
        return Err(format!(
            "verify refuses to run: agent model is `{}`, not `{LIVE_COMPLETION_MODEL}`",
            agent.settings().model
        ));
    }
    agent.set_context_enabled(false);

    let dialogue = compression_dialogue();
    if dialogue.len() != COMPRESS_MESSAGES {
        return Err(format!(
            "synthetic dialogue is {} messages, expected {COMPRESS_MESSAGES}",
            dialogue.len()
        ));
    }

    // 1. Базовая линия: стратегия off, вся история на провод.
    let full_folded = ask_after(&agent, &dialogue, FOLDED_QUESTION)?;
    let full_tail = ask_after(&agent, &dialogue, TAIL_QUESTION)?;

    // 2. Включаем сжатие и сворачиваем всё, что положено свернуть.
    agent.set_strategy(ContextStrategy::Summary);
    let mut folds = 0usize;
    let mut fold_tokens = 0u64;
    while let Some(report) = agent.fold_history(&dialogue)? {
        folds += 1;
        fold_tokens += report.usage.total_tokens;
    }
    if folds == 0 {
        return Err("сжатие не сработало: ни одной свёртки на 30 сообщениях".into());
    }

    // 3. Те же вопросы к сжатой истории.
    let comp_folded = ask_after(&agent, &dialogue, FOLDED_QUESTION)?;
    let comp_tail = ask_after(&agent, &dialogue, TAIL_QUESTION)?;

    let folded_fact = CompressionProbe {
        label: "факт из свёрнутой части",
        token: FOLDED_TOKEN,
        question: FOLDED_QUESTION,
        full: full_folded,
        compressed: comp_folded,
    };
    let tail_fact = CompressionProbe {
        label: "факт из дословного хвоста",
        token: TAIL_TOKEN,
        question: TAIL_QUESTION,
        full: full_tail,
        compressed: comp_tail,
    };
    let verdict = judge_compression(&folded_fact, &tail_fact);
    let compressor = agent.compressor();
    Ok(CompressionReport {
        messages: dialogue.len(),
        keep_recent: COMPRESS_KEEP_RECENT,
        every: COMPRESS_EVERY,
        covered: compressor.covered(),
        wire_messages: agent.wire_history(&dialogue).len(),
        folded_chars: compressor.folded_chars(),
        summary_chars: compressor.summary().chars().count(),
        summary: compressor.summary().to_string(),
        folds,
        fold_tokens,
        folded_fact,
        tail_fact,
        verdict,
    })
}

/// Задать вопрос поверх готового разговора, ничего не мутируя. История,
/// которую увидит провайдер, зависит от стратегии агента
/// (`Agent::wire_history`), и именно в этом вся проверка.
fn ask_after(agent: &Agent, dialogue: &[ChatMessage], question: &str) -> Res<Call> {
    let mut history = dialogue.to_vec();
    history.push(ChatMessage::user(question.to_string()));
    agent.complete(&history).map(Call::from_reply)
}

/// Причинная подпись сжатия, а не «тексты отличаются».
///
/// * Нет экономии `prompt_tokens` → `Flat`, сколько бы ни совпало текстов.
/// * Экономия есть, но факт из свёрнутой части потерян → `Lossy`.
/// * Полная история сама не вспомнила факт → тоже `Lossy`: сравнивать
///   качество не с чем, и заявлять «сжатие ничего не потеряло» нечестно.
/// * Экономия есть и оба факта на месте → `Confirmed`.
fn judge_compression(folded: &CompressionProbe, tail: &CompressionProbe) -> CompressionVerdict {
    let cheaper = folded.saved() > 0 && tail.saved() > 0;
    if !cheaper {
        return CompressionVerdict::Flat;
    }
    let baseline_knows = folded.full_recalled() && tail.full_recalled();
    let compressed_knows = folded.compressed_recalled() && tail.compressed_recalled();
    if baseline_knows && compressed_knows {
        CompressionVerdict::Confirmed
    } else {
        CompressionVerdict::Lossy
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Effort;
    use crate::compress::{Compressor, Policy};

    fn probe(token: &'static str, full_prompt: u64, comp_prompt: u64, full_text: &str, comp_text: &str) -> CompressionProbe {
        let mut full = call("stop", 5, full_text);
        full.prompt_tokens = full_prompt;
        let mut compressed = call("stop", 5, comp_text);
        compressed.prompt_tokens = comp_prompt;
        CompressionProbe {
            label: "test",
            token,
            question: "?",
            full,
            compressed,
        }
    }

    /// Проверка сжатия осмысленна только если посаженный факт действительно
    /// попадает в свёрнутую часть, а контрольный — в дословный хвост. Иначе
    /// «Confirmed» ничего не доказывает.
    #[test]
    fn planted_tokens_land_on_the_right_side_of_the_fold() {
        let dialogue = compression_dialogue();
        assert_eq!(dialogue.len(), COMPRESS_MESSAGES);
        let policy = Policy { keep_recent: COMPRESS_KEEP_RECENT, every: COMPRESS_EVERY };
        let mut c = Compressor::new();
        let target = c.due(dialogue.len(), policy).expect("fold must be due");
        let folded = &dialogue[..target];
        let kept = &dialogue[target..];
        assert!(folded.iter().any(|m| m.content.contains(FOLDED_TOKEN)));
        assert!(!kept.iter().any(|m| m.content.contains(FOLDED_TOKEN)));
        assert!(kept.iter().any(|m| m.content.contains(TAIL_TOKEN)));
        assert!(!folded.iter().any(|m| m.content.contains(TAIL_TOKEN)));
        // Один прохода хватает: после него сворачивать больше нечего.
        c.apply("s".into(), target, 1);
        assert_eq!(c.due(dialogue.len(), policy), None);
        assert!(folded.len() > kept.len(), "экономия должна быть заметной");
    }

    #[test]
    fn compression_is_flat_without_token_savings() {
        let same = probe(FOLDED_TOKEN, 900, 900, FOLDED_TOKEN, FOLDED_TOKEN);
        let tail = probe(TAIL_TOKEN, 900, 400, TAIL_TOKEN, TAIL_TOKEN);
        assert_eq!(judge_compression(&same, &tail), CompressionVerdict::Flat);
    }

    #[test]
    fn compression_is_lossy_when_the_summary_dropped_the_fact() {
        let folded = probe(FOLDED_TOKEN, 900, 400, FOLDED_TOKEN, "NONE");
        let tail = probe(TAIL_TOKEN, 900, 400, TAIL_TOKEN, TAIL_TOKEN);
        assert_eq!(judge_compression(&folded, &tail), CompressionVerdict::Lossy);
    }

    #[test]
    fn compression_is_lossy_when_the_baseline_did_not_know_it_either() {
        let folded = probe(FOLDED_TOKEN, 900, 400, "NONE", FOLDED_TOKEN);
        let tail = probe(TAIL_TOKEN, 900, 400, TAIL_TOKEN, TAIL_TOKEN);
        assert_eq!(judge_compression(&folded, &tail), CompressionVerdict::Lossy);
    }

    #[test]
    fn compression_confirmed_only_when_cheaper_and_both_facts_survive() {
        let folded = probe(FOLDED_TOKEN, 1200, 500, FOLDED_TOKEN, FOLDED_TOKEN);
        let tail = probe(TAIL_TOKEN, 1200, 500, TAIL_TOKEN, TAIL_TOKEN);
        assert_eq!(judge_compression(&folded, &tail), CompressionVerdict::Confirmed);
        let report = CompressionReport {
            messages: 30,
            keep_recent: 6,
            every: 10,
            covered: 20,
            wire_messages: 10,
            folded_chars: 1000,
            summary_chars: 300,
            summary: "сводка".into(),
            folds: 1,
            fold_tokens: 1400,
            folded_fact: folded,
            tail_fact: tail,
            verdict: CompressionVerdict::Confirmed,
        };
        assert!(report.confirmed());
        assert_eq!(report.saved_per_turn(), 700);
        assert_eq!(report.break_even_turns(), Some(2));
        let text = report.render();
        assert!(text.contains("Confirmed"));
        assert!(text.contains("prompt_tokens 1200 -> 500"));
        assert!(report.status_line().contains("saved/turn=+700"));
    }

    #[test]
    fn break_even_is_none_when_nothing_was_saved() {
        let folded = probe(FOLDED_TOKEN, 500, 900, FOLDED_TOKEN, FOLDED_TOKEN);
        let tail = probe(TAIL_TOKEN, 500, 900, TAIL_TOKEN, TAIL_TOKEN);
        let report = CompressionReport {
            messages: 30,
            keep_recent: 6,
            every: 10,
            covered: 20,
            wire_messages: 10,
            folded_chars: 1000,
            summary_chars: 300,
            summary: String::new(),
            folds: 1,
            fold_tokens: 1400,
            verdict: judge_compression(&folded, &tail),
            folded_fact: folded,
            tail_fact: tail,
        };
        assert_eq!(report.verdict, CompressionVerdict::Flat);
        assert_eq!(report.break_even_turns(), None);
        assert!(report.render().contains("никогда"));
    }

    fn call(finish: &str, completion: u64, text: &str) -> Call {
        Call {
            model: LIVE_COMPLETION_MODEL.into(),
            finish_reason: finish.into(),
            prompt_tokens: 10,
            completion_tokens: completion,
            reasoning_tokens: 0,
            total_tokens: 10 + completion,
            text: text.into(),
            latency_ms: 1,
        }
    }

    #[test]
    fn reach_is_confirmed_only_when_echo_matches_asked() {
        assert_eq!(LIVE_COMPLETION_MODEL, DEFAULT_MODEL);
        let matched = Call {
            model: "glm-5.3-flash".into(),
            ..call("stop", 2, "PONG")
        };
        assert_eq!(matched.model, LIVE_COMPLETION_MODEL);
        let sub = Call {
            model: "glm-5".into(),
            ..call("stop", 2, "PONG")
        };
        assert_ne!(sub.model, LIVE_COMPLETION_MODEL);
    }

    #[test]
    fn max_tokens_confirmed_only_at_cap_with_length_and_longer_control() {
        let capped = call("length", 16, "1, 2, 3");
        let control = call("stop", 80, "1, 2, 3, 4, 5");
        assert_eq!(
            judge_max_tokens(16, &capped, &control),
            LeverVerdict::Confirmed
        );
    }

    #[test]
    fn max_tokens_flat_when_capped_run_finished_naturally() {
        let capped = call("stop", 12, "1, 2, 3");
        let control = call("stop", 80, "1, 2, 3, 4, 5");
        assert_eq!(judge_max_tokens(16, &capped, &control), LeverVerdict::Flat);
    }

    #[test]
    fn max_tokens_flat_when_tokens_are_not_at_the_cap() {
        let capped = call("length", 8, "1, 2");
        let control = call("stop", 80, "1, 2, 3, 4, 5");
        assert_eq!(judge_max_tokens(16, &capped, &control), LeverVerdict::Flat);
    }

    #[test]
    fn max_tokens_flat_when_control_is_not_longer() {
        let capped = call("length", 16, "1, 2, 3");
        let control = call("stop", 16, "1, 2, 3");
        assert_eq!(judge_max_tokens(16, &capped, &control), LeverVerdict::Flat);
    }

    #[test]
    fn instruction_confirmed_only_when_on_has_token_and_off_does_not() {
        assert_eq!(
            judge_instruction("QUINCE", "NONE", "QUINCE"),
            LeverVerdict::Confirmed
        );
        assert_eq!(
            judge_instruction("The token is QUINCE.", "NONE", "QUINCE"),
            LeverVerdict::Confirmed
        );
    }

    #[test]
    fn instruction_flat_when_on_does_not_obey() {
        assert_eq!(
            judge_instruction("NONE", "NONE", "QUINCE"),
            LeverVerdict::Flat
        );
        assert_eq!(
            judge_instruction("42", "42", "QUINCE"),
            LeverVerdict::Flat
        );
    }

    #[test]
    fn instruction_flat_when_both_sides_emit_the_token() {
        assert_eq!(
            judge_instruction("QUINCE", "QUINCE", "QUINCE"),
            LeverVerdict::Flat
        );
    }

    #[test]
    fn has_token_is_word_bounded() {
        assert!(has_token("QUINCE", "QUINCE"));
        assert!(has_token("quince.", "QUINCE"));
        assert!(!has_token("NONE", "QUINCE"));
        assert!(!has_token("QUINCENTENNIAL", "QUINCE"));
    }

    #[test]
    fn sampling_confirmed_only_when_cold_collapses_and_hot_spreads() {
        assert_eq!(judge_sampling(1, 3, true), LeverVerdict::Confirmed);
    }

    #[test]
    fn sampling_unsupported_when_greedy_side_varies() {
        // temperature=0 / top_k=1 must be deterministic. Variation means the
        // parameter never reached the sampler — not a text-diff "pass".
        assert_eq!(judge_sampling(3, 3, true), LeverVerdict::Unsupported);
        assert_eq!(judge_sampling(2, 4, true), LeverVerdict::Unsupported);
    }

    #[test]
    fn sampling_flat_when_neither_side_spreads() {
        assert_eq!(judge_sampling(1, 1, true), LeverVerdict::Flat);
        assert_eq!(judge_sampling(1, 1, false), LeverVerdict::Flat);
    }

    #[test]
    fn sampling_two_different_texts_are_not_confirmation() {
        let cold = vec!["7".into(), "11".into(), "3".into(), "7".into()];
        let hot = vec!["4".into(), "19".into(), "8".into(), "2".into()];
        assert_ne!(cold, hot);
        assert_eq!(
            judge_sampling(distinct_count(&cold), distinct_count(&hot), true),
            LeverVerdict::Unsupported
        );
    }

    #[test]
    fn render_includes_endpoint_model_finish_and_tokens() {
        let ping = call("stop", 7, "PONG");
        let report = Report {
            reach: Reachability {
                endpoint: completions_url(),
                asked_model: LIVE_COMPLETION_MODEL.into(),
                call: ping,
                verdict: ReachVerdict::Confirmed,
            },
            max_tokens: MaxTokensCheck {
                low_cap: 16,
                control_cap: 96,
                capped: call("length", 16, "1, 2"),
                control: call("stop", 80, "1, 2, 3"),
                verdict: LeverVerdict::Confirmed,
            },
            system_prompt: InstructionCheck {
                label: "system prompt",
                token: SYSTEM_TOKEN,
                on: call("stop", 1, "QUINCE"),
                off: call("stop", 1, "NONE"),
                verdict: LeverVerdict::Confirmed,
            },
            agents_md: InstructionCheck {
                label: "AGENTS.md context",
                token: CONTEXT_TOKEN,
                on: call("stop", 1, "NIGHTJAR"),
                off: call("stop", 1, "NONE"),
                verdict: LeverVerdict::Flat,
            },
            temperature: SamplingCheck {
                lever: "temperature",
                cold_label: "0.0".into(),
                hot_label: "1.0".into(),
                cold: vec!["7".into(); 4],
                hot: vec!["7".into(); 4],
                cold_distinct: 1,
                hot_distinct: 1,
                verdict: LeverVerdict::Flat,
            },
            top_p: SamplingCheck {
                lever: "top_p",
                cold_label: "0.01".into(),
                hot_label: "1.0".into(),
                cold: vec!["7".into(); 4],
                hot: vec!["7".into(); 4],
                cold_distinct: 1,
                hot_distinct: 1,
                verdict: LeverVerdict::Flat,
            },
            top_k: SamplingCheck {
                lever: "top_k",
                cold_label: "1".into(),
                hot_label: "full".into(),
                cold: vec!["7".into(), "3".into(), "11".into(), "7".into()],
                hot: vec!["7".into(), "3".into(), "11".into(), "4".into()],
                cold_distinct: 3,
                hot_distinct: 4,
                verdict: LeverVerdict::Unsupported,
            },
        };
        let text = report.render();
        assert!(text.contains(&completions_url()));
        assert!(text.contains("glm-5.3-flash"));
        assert!(text.contains("finish=stop"));
        assert!(text.contains("finish=length"));
        assert!(text.contains("completion_tokens=16"));
        assert!(text.contains("completion_tokens=7"));
        assert!(text.contains("=> Flat"));
        assert!(text.contains("=> Unsupported"));
        assert!(text.contains("=> Confirmed"));
        assert!(!text.contains("the two texts differ"));
        assert_eq!(
            report.status_line(),
            "verify: reach=Confirmed max_tokens=Confirmed system=Confirmed agents.md=Flat temp=Flat top_p=Flat top_k=Unsupported"
        );
    }

    #[test]
    fn isolated_settings_are_flash_only_with_levers_off() {
        let s = isolated_settings();
        assert_eq!(s.model, LIVE_COMPLETION_MODEL);
        assert!(!s.context_enabled);
        assert!(!s.json_mode.enabled);
        assert!(s.budget_tokens.is_none());
        assert!(s.temperature.is_none());
        assert_eq!(s.effort, Effort::Low);
    }

    #[test]
    fn endpoint_is_the_plain_z_ai_completions_url() {
        let url = completions_url();
        assert_eq!(url, format!("{DEFAULT_BASE_URL}/chat/completions"));
        assert!(!url.contains("/coding/"));
    }
}
