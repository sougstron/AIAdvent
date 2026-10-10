//! Live self-test that the agent reaches z.ai and that configured levers
//! actually arrive at the provider.
//!
//! A "verified" claim here is a causal signature, not "the two texts differ":
//! sampling alone makes unconstrained calls differ. `Flat` / `Unsupported`
//! are valid, preferred outcomes when a parameter is ignored or damped.
//! Every live completion uses `glm-5.3-flash` only.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::agent::{Agent, Reply};
use crate::api::{self, ChatMessage, DEFAULT_BASE_URL, LIVE_COMPLETION_MODEL};
use crate::branch::BranchStore;
use crate::config::{self, ContextStrategy, Res, Settings, DEFAULT_MODEL};
use crate::memory::{self, Layer, MemoryStore};
use crate::profile;
use crate::session::StoredMessage;
use crate::todo::{self, TaskState};

const PING_PROMPT: &str = "Reply with the single word PONG.";
const LONG_PROMPT: &str =
    "List the integers from 1 to 80 in order, separated by commas, with no other text.";
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
    guard_verify_model(agent.settings())?;
    let history = [ChatMessage::user(prompt)];
    agent.complete(&history).map(Call::from_reply)
}

/// Денежный guard для verify — тот же, что и на отправке (`api::guard_live_model`):
/// `glm-5.3-flash`, `deepseek-flash` и любой OpenRouter `:free`. Ослабления
/// здесь нет: расширился только набор моделей, на которых можно гонять
/// проверку, а не правило «что разрешено звать живьём».
fn guard_verify_model(settings: &Settings) -> Res<()> {
    let provider = config::provider_of(&settings.model)
        .ok_or_else(|| config::catalog_error(&settings.model))?;
    api::guard_live_model(provider, &settings.model)
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
fn judge_sampling(
    cold_distinct: usize,
    hot_distinct: usize,
    cold_must_collapse: bool,
) -> LeverVerdict {
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

/// То же, что [`has_token`], но по словам Unicode: посаженные слова проверок
/// персонализации — русские («Евгений»), а `has_token` режет строку только по
/// ASCII-символам и на кириллице сравнивает не то.
fn has_word(text: &str, token: &str) -> bool {
    let needle = token.to_lowercase();
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| w == needle)
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
pub fn run_compression(model: Option<&str>) -> Res<CompressionReport> {
    let mut settings = isolated_settings();
    settings.keep_recent = COMPRESS_KEEP_RECENT;
    settings.summarize_every = COMPRESS_EVERY;
    settings.context_strategy = ContextStrategy::Off;
    if let Some(m) = model {
        settings.model = m.to_string();
    }
    guard_verify_model(&settings)?;
    let mut agent = Agent::new(settings)?;
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

// ------------------------------------------- стратегии управления контекстом

/// Токены, которые сажаются в синтетические разговоры проверок стратегий.
pub const OLD_TOKEN: &str = "NIGHTJAR7741";
pub const RECENT_TOKEN: &str = "KESTREL9120";
pub const SHARED_TOKEN: &str = "HERON4413";
pub const ALPHA_TOKEN: &str = "ALPACA5567";
pub const BETA_TOKEN: &str = "BADGER8802";

const WINDOW_KEEP: usize = 6;
const FACTS_KEEP: usize = 4;
/// Пауза перед единственным ретраем на HTTP 429. Бесплатный тариф
/// OpenRouter — около 20 запросов в минуту.
const RATE_LIMIT_PAUSE: std::time::Duration = std::time::Duration::from_secs(25);

const OLD_QUESTION: &str =
    "Какой код доступа к стенду я называл? Ответь одним словом — только кодом. Если не знаешь, ответь NONE.";
const RECENT_QUESTION: &str =
    "Какой код резервного канала я называл? Ответь одним словом — только кодом. Если не знаешь, ответь NONE.";
const ALL_CODES_QUESTION: &str =
    "Перечисли через запятую ВСЕ кодовые слова, которые я называл в этом разговоре. Только сами коды, без пояснений. Если ни одного — ответь NONE.";

/// Что именно проверяем.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ContextCheck {
    Window,
    Facts,
    Branch,
}

impl ContextCheck {
    pub const ALL: [ContextCheck; 3] = [
        ContextCheck::Window,
        ContextCheck::Facts,
        ContextCheck::Branch,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ContextCheck::Window => "window",
            ContextCheck::Facts => "facts",
            ContextCheck::Branch => "branch",
        }
    }

    /// `window|facts|branch|all`.
    pub fn parse(s: &str) -> Res<Vec<ContextCheck>> {
        match s.trim().to_ascii_lowercase().as_str() {
            "all" | "" => Ok(ContextCheck::ALL.to_vec()),
            "window" | "sliding" => Ok(vec![ContextCheck::Window]),
            "facts" | "kv" => Ok(vec![ContextCheck::Facts]),
            "branch" | "branching" => Ok(vec![ContextCheck::Branch]),
            other => Err(format!(
                "unknown context check `{other}`; expected window, facts, branch or all"
            )),
        }
    }
}

/// Как разрешилась проверка стратегии.
///
/// `Leaky` и `Flat` — такие же честные результаты, как `Confirmed`: они
/// означают, что подписи нет, а не что сломался прогон. `Inconclusive` —
/// база сама не знала посаженного факта, сравнивать не с чем.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ContextVerdict {
    Confirmed,
    Leaky,
    Flat,
    Inconclusive,
}

impl ContextVerdict {
    const fn as_str(self) -> &'static str {
        match self {
            ContextVerdict::Confirmed => "Confirmed",
            ContextVerdict::Leaky => "Leaky",
            ContextVerdict::Flat => "Flat",
            ContextVerdict::Inconclusive => "Inconclusive",
        }
    }
}

/// Один живой вопрос в рамках проверки стратегии.
#[derive(Clone, Debug)]
pub struct ContextProbe {
    pub label: String,
    pub token: &'static str,
    pub call: Call,
}

impl ContextProbe {
    pub fn recalled(&self) -> bool {
        has_token(&self.call.text, self.token)
    }

    fn line(&self) -> String {
        format!(
            "[{}] {}\n  {}\n  помнит {}: {}",
            self.label,
            self.call.line(),
            clip(&self.call.text, 120),
            self.token,
            self.recalled()
        )
    }
}

#[derive(Clone, Debug)]
pub struct WindowReport {
    pub model: String,
    pub asked_model: String,
    pub messages: usize,
    pub keep_recent: usize,
    pub wire_messages: usize,
    pub calls: usize,
    /// База (`off`): вся история.
    pub base_old: ContextProbe,
    pub base_recent: ContextProbe,
    /// Окно: последние `keep_recent`.
    pub win_old: ContextProbe,
    pub win_recent: ContextProbe,
    pub verdict: ContextVerdict,
}

#[derive(Clone, Debug)]
pub struct FactsReport {
    pub model: String,
    pub asked_model: String,
    pub messages: usize,
    pub keep_recent: usize,
    pub calls: usize,
    pub extractor_calls: usize,
    pub extractor_errors: Vec<String>,
    pub facts_block: String,
    pub fact_count: usize,
    /// Три конфигурации на одном разговоре.
    pub base: ContextProbe,
    pub window: ContextProbe,
    pub facts: ContextProbe,
    pub verdict: ContextVerdict,
}

#[derive(Clone, Debug)]
pub struct BranchReport {
    pub model: String,
    pub asked_model: String,
    pub calls: usize,
    pub branches: String,
    /// Линейный разговор, содержащий обе ветки — цена «без веток».
    pub linear: Call,
    pub linear_len: usize,
    pub alpha: Call,
    pub alpha_len: usize,
    pub beta: Call,
    pub beta_len: usize,
    pub verdict: ContextVerdict,
}

/// Отчёт одной проверки стратегии.
#[derive(Clone, Debug)]
pub enum ContextReport {
    Window(WindowReport),
    Facts(FactsReport),
    Branch(BranchReport),
}

impl ContextReport {
    pub fn verdict(&self) -> ContextVerdict {
        match self {
            ContextReport::Window(r) => r.verdict,
            ContextReport::Facts(r) => r.verdict,
            ContextReport::Branch(r) => r.verdict,
        }
    }

    pub fn confirmed(&self) -> bool {
        self.verdict() == ContextVerdict::Confirmed
    }

    pub fn calls(&self) -> usize {
        match self {
            ContextReport::Window(r) => r.calls,
            ContextReport::Facts(r) => r.calls,
            ContextReport::Branch(r) => r.calls,
        }
    }

    pub fn status_line(&self) -> String {
        match self {
            ContextReport::Window(r) => format!(
                "window: verdict={} старое забыто={} свежее помнит={} prompt_tokens {}->{}",
                r.verdict.as_str(),
                !r.win_old.recalled(),
                r.win_recent.recalled(),
                r.base_old.call.prompt_tokens,
                r.win_old.call.prompt_tokens
            ),
            ContextReport::Facts(r) => format!(
                "facts: verdict={} факт выжил={} у окна без фактов={} prompt_tokens {}->{}",
                r.verdict.as_str(),
                r.facts.recalled(),
                r.window.recalled(),
                r.base.call.prompt_tokens,
                r.facts.call.prompt_tokens
            ),
            ContextReport::Branch(r) => format!(
                "branch: verdict={} alpha={} beta={} prompt_tokens линейно {} против веток {}/{}",
                r.verdict.as_str(),
                clip(&r.alpha.text, 40),
                clip(&r.beta.text, 40),
                r.linear.prompt_tokens,
                r.alpha.prompt_tokens,
                r.beta.prompt_tokens
            ),
        }
    }

    pub fn render(&self) -> String {
        match self {
            ContextReport::Window(r) => {
                let mut out = format!(
                    "== стратегия window (sliding window) ==\nмодель: просили {}, ответила {}\nразговор {} сообщений, keep_recent={}, на проводе {} + вопрос; живых вызовов {}\n\n",
                    r.asked_model, model_or_q(&r.model), r.messages, r.keep_recent, r.wire_messages, r.calls
                );
                out.push_str(&format!(
                    "-- факт из начала разговора (token={OLD_TOKEN}) --\n{}\n{}\n\n",
                    r.base_old.line(),
                    r.win_old.line()
                ));
                out.push_str(&format!(
                    "-- факт из хвоста (token={RECENT_TOKEN}) --\n{}\n{}\n\n",
                    r.base_recent.line(),
                    r.win_recent.line()
                ));
                out.push_str(&format!(
                    "prompt_tokens: {} -> {} ({:+})\n",
                    r.base_old.call.prompt_tokens,
                    r.win_old.call.prompt_tokens,
                    r.win_old.call.prompt_tokens as i64 - r.base_old.call.prompt_tokens as i64
                ));
                out.push_str(&format!("=> {}\n", r.verdict.as_str()));
                out.push_str(
                    "Confirmed требует причинной подписи: prompt_tokens упали, свежий факт помнится И старый честно ЗАБЫТ. \
Leaky — окно не режет (старое всё ещё помнится). Flat — экономии нет. Inconclusive — полная история сама не знала факта.\n",
                );
                out
            }
            ContextReport::Facts(r) => {
                let mut out = format!(
                    "== стратегия facts (sticky key-value память) ==\nмодель: просили {}, ответила {}\nразговор {} сообщений, keep_recent={}, живых вызовов {} (из них экстрактор {})\n\n",
                    r.asked_model, model_or_q(&r.model), r.messages, r.keep_recent, r.calls, r.extractor_calls
                );
                out.push_str(&format!(
                    "-- блок фактов ({} шт.) --\n{}\n\n",
                    r.fact_count,
                    clip(&r.facts_block, 1200)
                ));
                if !r.extractor_errors.is_empty() {
                    out.push_str(&format!(
                        "ошибки разбора экстрактора: {}\n\n",
                        r.extractor_errors.join("; ")
                    ));
                }
                out.push_str(&format!(
                    "-- один и тот же вопрос (token={OLD_TOKEN}) --\n{}\n{}\n{}\n\n",
                    r.base.line(),
                    r.window.line(),
                    r.facts.line()
                ));
                out.push_str(&format!(
                    "prompt_tokens: off {} | window {} | facts {}\n",
                    r.base.call.prompt_tokens,
                    r.window.call.prompt_tokens,
                    r.facts.call.prompt_tokens
                ));
                out.push_str(&format!("=> {}\n", r.verdict.as_str()));
                out.push_str(
                    "Confirmed требует, чтобы окно БЕЗ фактов факт потеряло, окно С фактами его вспомнило и вышло дешевле полной истории: \
разница между этими двумя прогонами — ровно блок фактов. Lossy/Flat/Inconclusive — честный результат, а не сломанный тест.\n",
                );
                out
            }
            ContextReport::Branch(r) => {
                let mut out = format!(
                    "== стратегия branch (ветки диалога) ==\nмодель: просили {}, ответила {}\nживых вызовов {}\n{}\n\n",
                    r.asked_model, model_or_q(&r.model), r.calls, r.branches
                );
                out.push_str(&format!(
                    "-- линейный разговор с обеими ветками ({} сообщений) --\n{}\n  {}\n\n",
                    r.linear_len,
                    r.linear.line(),
                    clip(&r.linear.text, 120)
                ));
                out.push_str(&format!(
                    "-- ветка alpha ({} сообщений; свой токен {ALPHA_TOKEN}) --\n{}\n  {}\n  знает SHARED={} ALPHA={} BETA={}\n\n",
                    r.alpha_len,
                    r.alpha.line(),
                    clip(&r.alpha.text, 120),
                    has_token(&r.alpha.text, SHARED_TOKEN),
                    has_token(&r.alpha.text, ALPHA_TOKEN),
                    has_token(&r.alpha.text, BETA_TOKEN)
                ));
                out.push_str(&format!(
                    "-- ветка beta ({} сообщений; свой токен {BETA_TOKEN}) --\n{}\n  {}\n  знает SHARED={} ALPHA={} BETA={}\n\n",
                    r.beta_len,
                    r.beta.line(),
                    clip(&r.beta.text, 120),
                    has_token(&r.beta.text, SHARED_TOKEN),
                    has_token(&r.beta.text, ALPHA_TOKEN),
                    has_token(&r.beta.text, BETA_TOKEN)
                ));
                out.push_str(&format!(
                    "prompt_tokens: линейно {} | alpha {} | beta {}\n",
                    r.linear.prompt_tokens, r.alpha.prompt_tokens, r.beta.prompt_tokens
                ));
                out.push_str(&format!("=> {}\n", r.verdict.as_str()));
                out.push_str(
                    "Confirmed требует непротекания: обе ветки знают общий токен, каждая знает только свой и НЕ знает чужой, \
и любая ветка дешевле линейного разговора с обеими. Leaky — чужой токен протёк (именно эту багу даёт общая на всё дерево память).\n",
                );
                out
            }
        }
    }
}

fn model_or_q(model: &str) -> &str {
    if model.is_empty() {
        "?"
    } else {
        model
    }
}

/// Настройки для проверки стратегий: изоляция от AGENTS.md и от системного
/// промпта плюс выбранная модель.
fn context_settings(model: Option<&str>) -> Res<Settings> {
    let mut settings = isolated_settings();
    if let Some(m) = model {
        settings.model = m.trim().to_string();
    }
    guard_verify_model(&settings)?;
    Ok(settings)
}

/// Один живой вопрос поверх готовой истории, с единственным ретраем на 429.
fn ask_call(agent: &Agent, history: &[ChatMessage], question: &str) -> Res<Call> {
    let mut full = history.to_vec();
    full.push(ChatMessage::user(question.to_string()));
    match agent.complete(&full) {
        Err(e) if is_rate_limited(&e) => {
            std::thread::sleep(RATE_LIMIT_PAUSE);
            agent.complete(&full).map(Call::from_reply)
        }
        other => other.map(Call::from_reply),
    }
}

fn is_rate_limited(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("429") || e.contains("rate limit") || e.contains("too many requests")
}

/// `--verify-context`: запускает выбранные проверки последовательно.
pub fn run_context(checks: &[ContextCheck], model: Option<&str>) -> Res<Vec<ContextReport>> {
    let mut out = Vec::new();
    for check in checks {
        out.push(match check {
            ContextCheck::Window => ContextReport::Window(run_window(model)?),
            ContextCheck::Facts => ContextReport::Facts(run_facts(model)?),
            ContextCheck::Branch => ContextReport::Branch(run_branch(model)?),
        });
    }
    Ok(out)
}

/// Проверка sliding window.
///
/// Тот же разговор задаётся дважды: со стратегией `off` (база знает всё) и с
/// окном на `keep_recent` сообщений. Доказательство именно в асимметрии:
/// свежий факт обязан помниться, а старый — обязан быть забыт. «Старое
/// забыто» здесь не провал, а подпись: она показывает, что окно реально
/// режет, а не что модель подглядела ответ.
pub fn run_window(model: Option<&str>) -> Res<WindowReport> {
    let mut settings = context_settings(model)?;
    settings.keep_recent = WINDOW_KEEP;
    settings.context_strategy = ContextStrategy::Off;
    let asked_model = settings.model.clone();
    let mut agent = Agent::new(settings)?;
    agent.set_context_enabled(false);

    let dialogue = compression_dialogue();
    let base_old = ContextProbe {
        label: "off".into(),
        token: OLD_TOKEN,
        call: ask_call(&agent, &dialogue, OLD_QUESTION)?,
    };
    let base_recent = ContextProbe {
        label: "off".into(),
        token: RECENT_TOKEN,
        call: ask_call(&agent, &dialogue, RECENT_QUESTION)?,
    };

    agent.set_strategy(ContextStrategy::Window);
    let win_old = ContextProbe {
        label: "window".into(),
        token: OLD_TOKEN,
        call: ask_call(&agent, &dialogue, OLD_QUESTION)?,
    };
    let win_recent = ContextProbe {
        label: "window".into(),
        token: RECENT_TOKEN,
        call: ask_call(&agent, &dialogue, RECENT_QUESTION)?,
    };

    let mut with_question = dialogue.clone();
    with_question.push(ChatMessage::user(OLD_QUESTION));
    let verdict = judge_window(&base_old, &base_recent, &win_old, &win_recent);
    Ok(WindowReport {
        model: win_old.call.model.clone(),
        asked_model,
        messages: dialogue.len(),
        keep_recent: WINDOW_KEEP,
        wire_messages: agent.wire_history(&with_question).len(),
        calls: 4,
        base_old,
        base_recent,
        win_old,
        win_recent,
        verdict,
    })
}

/// Причинная подпись окна.
///
/// * `prompt_tokens` не упали (или провайдер не отдал usage) → `Flat`.
/// * Полная история сама не вспомнила факты → `Inconclusive`.
/// * Старый факт всё ещё помнится → `Leaky`: окно не режет.
/// * Дешевле, свежее помнится, старое забыто → `Confirmed`.
fn judge_window(
    base_old: &ContextProbe,
    base_recent: &ContextProbe,
    win_old: &ContextProbe,
    win_recent: &ContextProbe,
) -> ContextVerdict {
    let usage_known = base_old.call.prompt_tokens > 0 && win_old.call.prompt_tokens > 0;
    let cheaper = win_old.call.prompt_tokens < base_old.call.prompt_tokens
        && win_recent.call.prompt_tokens < base_recent.call.prompt_tokens;
    if !usage_known || !cheaper {
        return ContextVerdict::Flat;
    }
    if !base_old.recalled() || !base_recent.recalled() {
        return ContextVerdict::Inconclusive;
    }
    if win_old.recalled() {
        return ContextVerdict::Leaky;
    }
    if win_recent.recalled() {
        ContextVerdict::Confirmed
    } else {
        ContextVerdict::Inconclusive
    }
}

/// Проверка sticky facts.
///
/// Три конфигурации на одном и том же разговоре: `off` (знает всё), `window`
/// (контроль — факт старше окна потерян) и `facts` (то же окно плюс блок
/// памяти). Разница между вторым и третьим прогоном — ровно блок фактов,
/// больше ничего, поэтому вспомненный в третьем факт приписать нечему, кроме
/// памяти.
pub fn run_facts(model: Option<&str>) -> Res<FactsReport> {
    let mut settings = context_settings(model)?;
    settings.keep_recent = FACTS_KEEP;
    settings.context_strategy = ContextStrategy::Off;
    let asked_model = settings.model.clone();
    let mut agent = Agent::new(settings)?;
    agent.set_context_enabled(false);

    let dialogue = facts_dialogue();
    let base = ContextProbe {
        label: "off".into(),
        token: OLD_TOKEN,
        call: ask_call(&agent, &dialogue, OLD_QUESTION)?,
    };

    agent.set_strategy(ContextStrategy::Window);
    let window = ContextProbe {
        label: "window (без фактов)".into(),
        token: OLD_TOKEN,
        call: ask_call(&agent, &dialogue, OLD_QUESTION)?,
    };

    // Экстрактор гоняем ровно так, как это делает TUI: после каждой реплики
    // пользователя, по текущим фактам и последней паре.
    agent.set_strategy(ContextStrategy::Facts);
    let mut extractor_calls = 0usize;
    let mut extractor_errors = Vec::new();
    for i in 0..dialogue.len() {
        if dialogue[i].role != crate::api::Role::User {
            continue;
        }
        extractor_calls += 1;
        if let Err(e) = agent.update_facts(&dialogue[..=i]) {
            extractor_errors.push(e);
        }
    }
    let facts = ContextProbe {
        label: "facts (окно + память)".into(),
        token: OLD_TOKEN,
        call: ask_call(&agent, &dialogue, OLD_QUESTION)?,
    };

    let verdict = judge_facts(&base, &window, &facts);
    Ok(FactsReport {
        model: facts.call.model.clone(),
        asked_model,
        messages: dialogue.len(),
        keep_recent: FACTS_KEEP,
        calls: 3 + extractor_calls,
        extractor_calls,
        extractor_errors,
        facts_block: agent.facts().block().unwrap_or_else(|| "(пусто)".into()),
        fact_count: agent.facts().len(),
        base,
        window,
        facts,
        verdict,
    })
}

/// Причинная подпись памяти фактов.
///
/// * Дороже полной истории (или usage неизвестен) → `Flat`: платить столько
///   же и помнить меньше — не стратегия.
/// * База сама не знала факта → `Inconclusive`.
/// * Окно без фактов факт помнит → `Leaky`: окно не резало, и проверка
///   ничего не доказывает про память.
/// * Окно потеряло, память вернула, и это дешевле базы → `Confirmed`.
fn judge_facts(base: &ContextProbe, window: &ContextProbe, facts: &ContextProbe) -> ContextVerdict {
    let usage_known = base.call.prompt_tokens > 0 && facts.call.prompt_tokens > 0;
    if !usage_known || facts.call.prompt_tokens >= base.call.prompt_tokens {
        return ContextVerdict::Flat;
    }
    if !base.recalled() {
        return ContextVerdict::Inconclusive;
    }
    if window.recalled() {
        return ContextVerdict::Leaky;
    }
    if facts.recalled() {
        ContextVerdict::Confirmed
    } else {
        ContextVerdict::Inconclusive
    }
}

/// Проверка веток.
///
/// Общий префикс с токеном `SHARED`, чекпойнт, две ветки: в одной посажен
/// `ALPHA`, в другой `BETA`. Дерево строит настоящий `BranchStore` — тот же,
/// что и в TUI, — поэтому проверяется реализация, а не отдельная модель для
/// теста.
pub fn run_branch(model: Option<&str>) -> Res<BranchReport> {
    let mut settings = context_settings(model)?;
    settings.context_strategy = ContextStrategy::Branch;
    let asked_model = settings.model.clone();
    let mut agent = Agent::new(settings)?;
    agent.set_context_enabled(false);

    let mut tree = BranchStore::new();
    for m in shared_prefix() {
        tree.push(m);
    }
    tree.checkpoint(Some("развилка"))?;
    tree.fork("alpha", Some("развилка"))?;
    tree.fork("beta", Some("развилка"))?;

    let empty = || {
        (
            crate::compress::Compressor::new(),
            crate::facts::FactStore::new(),
        )
    };
    tree.switch_memory("alpha", empty())?;
    for m in branch_tail("alpha", ALPHA_TOKEN) {
        tree.push(m);
    }
    let alpha_path = to_messages(&tree.path());

    tree.switch_memory("beta", empty())?;
    for m in branch_tail("beta", BETA_TOKEN) {
        tree.push(m);
    }
    let beta_path = to_messages(&tree.path());

    // Контроль «без веток»: линейный разговор, в котором обе ветки лежат
    // подряд. Именно его цену экономит изоляция.
    let mut linear = alpha_path.clone();
    linear.extend(beta_path[shared_prefix().len()..].iter().cloned());

    let linear_call = ask_call(&agent, &linear, ALL_CODES_QUESTION)?;
    let alpha_call = ask_call(&agent, &alpha_path, ALL_CODES_QUESTION)?;
    let beta_call = ask_call(&agent, &beta_path, ALL_CODES_QUESTION)?;

    let verdict = judge_branch(&linear_call, &alpha_call, &beta_call);
    Ok(BranchReport {
        model: alpha_call.model.clone(),
        asked_model,
        calls: 3,
        branches: tree.listing(),
        linear_len: linear.len(),
        alpha_len: alpha_path.len(),
        beta_len: beta_path.len(),
        linear: linear_call,
        alpha: alpha_call,
        beta: beta_call,
        verdict,
    })
}

/// Причинная подпись веток — непротекание, а не «тексты отличаются».
///
/// * Ветка не дешевле линейного разговора с обеими (или usage неизвестен)
///   → `Flat`.
/// * Обе ветки обязаны знать общий префикс; если нет — `Inconclusive`.
/// * Чужой токен виден в ветке → `Leaky`. Это ровно та бага, которую даёт
///   общая на всё дерево память.
/// * Каждая ветка знает свой токен и не знает чужой → `Confirmed`.
fn judge_branch(linear: &Call, alpha: &Call, beta: &Call) -> ContextVerdict {
    let usage_known = linear.prompt_tokens > 0 && alpha.prompt_tokens > 0 && beta.prompt_tokens > 0;
    let cheaper =
        alpha.prompt_tokens < linear.prompt_tokens && beta.prompt_tokens < linear.prompt_tokens;
    if !usage_known || !cheaper {
        return ContextVerdict::Flat;
    }
    let shared = has_token(&alpha.text, SHARED_TOKEN) && has_token(&beta.text, SHARED_TOKEN);
    if !shared {
        return ContextVerdict::Inconclusive;
    }
    if has_token(&alpha.text, BETA_TOKEN) || has_token(&beta.text, ALPHA_TOKEN) {
        return ContextVerdict::Leaky;
    }
    if has_token(&alpha.text, ALPHA_TOKEN) && has_token(&beta.text, BETA_TOKEN) {
        ContextVerdict::Confirmed
    } else {
        ContextVerdict::Inconclusive
    }
}

/// Короткий разговор для проверки фактов: 14 сообщений, посаженный в самом
/// начале код и достаточно наполнителя, чтобы при `keep_recent=4` он ушёл за
/// окно. Короче, чем разговор для сжатия, — бесплатный тариф OpenRouter
/// считает запросы, а не только токены.
pub fn facts_dialogue() -> Vec<ChatMessage> {
    let topics = [
        (
            "Цель — подготовить нагрузочный стенд сервиса заказов за две недели, команда маленькая.",
            "Понял: стенд для сервиса заказов, срок две недели, команда небольшая.",
        ),
        (
            "Стек: Rust, PostgreSQL 16 с репликой на чтение, очередь NATS, всё в Kubernetes.",
            "Записал стек: Rust, PostgreSQL 16 с репликой, NATS, Kubernetes.",
        ),
        (
            "Целевая нагрузка 4000 запросов в секунду, 85 процентов чтение, пик вечером.",
            "Цель 4000 rps с перекосом 85/15 и вечерним пиком зафиксирована.",
        ),
        (
            "SLO: p99 чтения 120 миллисекунд, записи 400. Выход за границы — стоп прогона.",
            "SLO записал: 120 мс на чтение, 400 мс на запись, нарушение останавливает прогон.",
        ),
        (
            "Отчёт нужен в понедельник утром: таблица сценариев и вывод, что чинить первым.",
            "Отчёт к понедельнику: таблица по сценариям и приоритет починки.",
        ),
    ];
    let mut history = vec![
        ChatMessage::user(format!(
            "Запомни: код доступа к стенду — {OLD_TOKEN}. Он понадобится в конце разговора."
        )),
        ChatMessage::assistant(format!("Запомнил: код доступа к стенду {OLD_TOKEN}.")),
    ];
    for (q, a) in topics {
        history.push(ChatMessage::user(q.to_string()));
        history.push(ChatMessage::assistant(a.to_string()));
    }
    history.push(ChatMessage::user(format!(
        "И ещё: код резервного канала — {RECENT_TOKEN}."
    )));
    history.push(ChatMessage::assistant(format!(
        "Запомнил: код резервного канала {RECENT_TOKEN}."
    )));
    history
}

/// Общий префикс обеих веток.
fn shared_prefix() -> Vec<StoredMessage> {
    vec![
        stored(
            "user",
            &format!(
                "Запомни общий код проекта — {SHARED_TOKEN}. Он относится ко всему разговору."
            ),
        ),
        stored(
            "assistant",
            &format!("Запомнил общий код проекта {SHARED_TOKEN}."),
        ),
        stored(
            "user",
            "Дальше мы разойдёмся на два варианта плана; общий код остаётся в силе.",
        ),
        stored(
            "assistant",
            "Хорошо, общий код держу; жду, какой вариант разбираем.",
        ),
    ]
}

/// Хвост одной ветки с её собственным кодом.
fn branch_tail(name: &str, token: &str) -> Vec<StoredMessage> {
    vec![
        stored(
            "user",
            &format!("Берём вариант {name}. Его код — {token}. Запомни именно этот код."),
        ),
        stored(
            "assistant",
            &format!("Принято: вариант {name}, код {token}."),
        ),
    ]
}

fn stored(role: &str, content: &str) -> StoredMessage {
    StoredMessage {
        role: role.into(),
        content: content.into(),
        interrupted: false,
    }
}

fn to_messages(path: &[StoredMessage]) -> Vec<ChatMessage> {
    path.iter().map(StoredMessage::to_chat_message).collect()
}


// ------------------------------------------------ модель памяти (задача 11)

/// Токены проверок памяти. Каждый посажен ровно в один слой, и вопрос про
/// него отвечается только из этого слоя.
pub const PROFILE_TOKEN: &str = "PELICAN3288";
pub const TASK_TOKEN: &str = "OTTER5514";

const MEMORY_KEEP: usize = 4;
const PROFILE_KEY: &str = "профиль.код";
const TASK_KEY: &str = "задача.код";
const PROFILE_QUESTION: &str = "Какой мой личный код я просил запомнить навсегда? Ответь одним словом — только кодом. Если не знаешь, ответь NONE.";
const TASK_QUESTION: &str = "Какой код текущей задачи я называл? Ответь одним словом — только кодом. Если не знаешь, ответь NONE.";

/// Что именно проверяем в модели памяти.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MemoryCheck {
    /// Маршрутизация: что в какой слой попадает и что от этого на диске.
    Routing,
    /// Влияние слоя на ответ: по блоку за раз.
    Influence,
    /// Разделение слоёв: смена задачи уносит рабочую память и не трогает
    /// долговременную.
    Isolation,
}

impl MemoryCheck {
    pub const ALL: [MemoryCheck; 3] = [
        MemoryCheck::Routing,
        MemoryCheck::Influence,
        MemoryCheck::Isolation,
    ];

    pub fn label(self) -> &'static str {
        match self {
            MemoryCheck::Routing => "routing",
            MemoryCheck::Influence => "influence",
            MemoryCheck::Isolation => "isolation",
        }
    }

    /// `routing|influence|isolation|all`.
    pub fn parse(s: &str) -> Res<Vec<MemoryCheck>> {
        match s.trim().to_ascii_lowercase().as_str() {
            "all" | "" => Ok(MemoryCheck::ALL.to_vec()),
            "routing" | "route" | "layers" => Ok(vec![MemoryCheck::Routing]),
            "influence" | "answers" => Ok(vec![MemoryCheck::Influence]),
            "isolation" | "separation" => Ok(vec![MemoryCheck::Isolation]),
            other => Err(format!(
                "unknown memory check `{other}`; expected routing, influence, isolation or all"
            )),
        }
    }
}

/// Одна запись, прошедшая через маршрутизатор.
#[derive(Clone, Debug)]
pub struct RouteCase {
    pub key: String,
    /// Слой, который назвал экстрактор (или `-`, если не называл).
    pub asked: String,
    pub landed: Layer,
    pub expected: Layer,
    pub reason: String,
}

impl RouteCase {
    fn ok(&self) -> bool {
        self.landed == self.expected
    }

    fn line(&self) -> String {
        format!(
            "{:<20} просили {:<8} → лёг в {:<8} ({}) {}",
            self.key,
            self.asked,
            self.landed.label(),
            self.reason,
            if self.ok() { "OK" } else { "НЕ ТУДА" }
        )
    }
}

/// Что реально лежит в файле слоя после раскладки.
#[derive(Clone, Debug)]
pub struct LayerFileView {
    pub layer: Layer,
    pub path: String,
    pub keys: Vec<String>,
}

impl RoutingReport {
    /// Сколько живых вызовов сделала проверка: с `--offline` — ноль.
    pub fn calls_live(&self) -> usize {
        self.live.as_ref().map(|l| l.calls).unwrap_or(0)
    }
}

#[derive(Clone, Debug)]
pub struct RoutingReport {
    /// Чистый маршрутизатор: фиксированный набор записей, ноль запросов.
    pub cases: Vec<RouteCase>,
    /// Файлы трёх слоёв, перечитанные с диска после раскладки.
    pub files: Vec<LayerFileView>,
    /// Живая часть: настоящий экстрактор на коротком разговоре.
    pub live: Option<LiveRouting>,
    pub verdict: ContextVerdict,
}

/// Живая половина проверки маршрутизации.
#[derive(Clone, Debug)]
pub struct LiveRouting {
    /// Модель, у которой спрашивали. Какая ответила, здесь не видно: вызов
    /// экстрактора живёт внутри `Agent::update_memory` и наружу отдаёт
    /// только раскладку — врать про «ответила X» ради красивой строки не
    /// станем.
    pub asked_model: String,
    pub calls: usize,
    pub errors: Vec<String>,
    /// Куда экстрактор разложил записи: (слой, ключ, причина).
    pub routes: Vec<(Layer, String, String)>,
    /// В каком слое оказался посаженный токен профиля / задачи.
    pub profile_layer: Option<Layer>,
    pub task_layer: Option<Layer>,
}

#[derive(Clone, Debug)]
pub struct InfluenceReport {
    pub model: String,
    pub asked_model: String,
    pub calls: usize,
    pub messages: usize,
    pub keep_recent: usize,
    pub layers: String,
    /// Окно без блоков памяти — контроль: оба токена должны быть забыты.
    pub window_profile: ContextProbe,
    pub window_task: ContextProbe,
    /// То же окно плюс три блока памяти.
    pub memory_profile: ContextProbe,
    pub memory_task: ContextProbe,
    /// То же самое, но долговременный слой снят. Разница с предыдущим
    /// прогоном — ровно один блок.
    pub without_long_profile: ContextProbe,
    pub without_long_task: ContextProbe,
    pub verdict: ContextVerdict,
}

#[derive(Clone, Debug)]
pub struct MemIsolationReport {
    pub model: String,
    pub asked_model: String,
    pub calls: usize,
    pub task_a: String,
    pub task_b: String,
    /// Задача A: знает оба кода.
    pub a_profile: ContextProbe,
    pub a_task: ContextProbe,
    /// Задача B: долговременный слой при ней, рабочий — чужой, пустой.
    pub b_profile: ContextProbe,
    pub b_task: ContextProbe,
    /// Файл рабочего слоя задачи A после переключения: данные не потеряны,
    /// они просто не на проводе.
    pub a_file: String,
    pub a_file_keeps_token: bool,
    pub verdict: ContextVerdict,
}

/// Отчёт одной проверки памяти.
#[derive(Clone, Debug)]
pub enum MemoryReport {
    Routing(RoutingReport),
    // Отчёты живых проверок толще офлайновой раскладки (шесть и четыре
    // вызова против нуля) — держим их за боксом, чтобы перечисление не
    // раздувалось до размера самого большого варианта.
    Influence(Box<InfluenceReport>),
    Isolation(Box<MemIsolationReport>),
}

impl MemoryReport {
    pub fn verdict(&self) -> ContextVerdict {
        match self {
            MemoryReport::Routing(r) => r.verdict,
            MemoryReport::Influence(r) => r.verdict,
            MemoryReport::Isolation(r) => r.verdict,
        }
    }

    pub fn confirmed(&self) -> bool {
        self.verdict() == ContextVerdict::Confirmed
    }

    pub fn calls(&self) -> usize {
        match self {
            MemoryReport::Routing(r) => r.calls_live(),
            MemoryReport::Influence(r) => r.calls,
            MemoryReport::Isolation(r) => r.calls,
        }
    }

    pub fn status_line(&self) -> String {
        match self {
            MemoryReport::Routing(r) => format!(
                "routing: verdict={} правильно разложено {}/{}{}",
                r.verdict.as_str(),
                r.cases.iter().filter(|c| c.ok()).count(),
                r.cases.len(),
                match &r.live {
                    Some(l) => format!(
                        ", живьём профиль→{} задача→{}",
                        l.profile_layer.map(|x| x.label()).unwrap_or("нигде"),
                        l.task_layer.map(|x| x.label()).unwrap_or("нигде")
                    ),
                    None => ", живая часть пропущена (--offline)".into(),
                }
            ),
            MemoryReport::Influence(r) => format!(
                "influence: verdict={} окно забыло={} память вернула={} без long-слоя профиль={} задача={}",
                r.verdict.as_str(),
                !r.window_profile.recalled() && !r.window_task.recalled(),
                r.memory_profile.recalled() && r.memory_task.recalled(),
                r.without_long_profile.recalled(),
                r.without_long_task.recalled()
            ),
            MemoryReport::Isolation(r) => format!(
                "isolation: verdict={} задача A (профиль={} задача={}) → задача B (профиль={} задача={}), файл A хранит код={}",
                r.verdict.as_str(),
                r.a_profile.recalled(),
                r.a_task.recalled(),
                r.b_profile.recalled(),
                r.b_task.recalled(),
                r.a_file_keeps_token
            ),
        }
    }

    pub fn render(&self) -> String {
        match self {
            MemoryReport::Routing(r) => {
                let mut out = String::from(
                    "== модель памяти: маршрутизация (что и куда сохраняется) ==\n\n-- чистый маршрутизатор, без сети --\n",
                );
                for c in &r.cases {
                    out.push_str(&format!("{}\n", c.line()));
                }
                out.push_str("\n-- файлы слоёв после раскладки --\n");
                for f in &r.files {
                    out.push_str(&format!(
                        "[{}] {}\n  ключи: {}\n",
                        f.layer.label(),
                        f.path,
                        if f.keys.is_empty() {
                            "(пусто)".to_string()
                        } else {
                            f.keys.join(", ")
                        }
                    ));
                }
                match &r.live {
                    Some(l) => {
                        out.push_str(&format!(
                            "\n-- живой экстрактор на модели {}: вызовов {} --\n",
                            l.asked_model, l.calls
                        ));
                        for (layer, key, reason) in &l.routes {
                            out.push_str(&format!("[{}] {key} — {reason}\n", layer.label()));
                        }
                        if !l.errors.is_empty() {
                            out.push_str(&format!("ошибки разбора: {}\n", l.errors.join("; ")));
                        }
                        out.push_str(&format!(
                            "код профиля ({PROFILE_TOKEN}) оказался в слое {}, код задачи ({TASK_TOKEN}) — в слое {}\n",
                            l.profile_layer.map(|x| x.label()).unwrap_or("нигде"),
                            l.task_layer.map(|x| x.label()).unwrap_or("нигде")
                        ));
                    }
                    None => out.push_str("\n-- живая часть пропущена (--offline) --\n"),
                }
                out.push_str(&format!("=> {}\n", r.verdict.as_str()));
                out.push_str(
                    "Confirmed требует, чтобы каждая запись легла в ожидаемый слой И чтобы это было видно в файле именно этого слоя, \
а живой экстрактор посадил код профиля в long, а код задачи — в working. Leaky — запись оказалась не в своём слое.\n",
                );
                out
            }
            MemoryReport::Influence(r) => {
                let mut out = format!(
                    "== модель памяти: влияние слоёв на ответ ==\nмодель: просили {}, ответила {}\nразговор {} сообщений, keep_recent={}, живых вызовов {}\nслои: {}\n\n",
                    r.asked_model,
                    model_or_q(&r.model),
                    r.messages,
                    r.keep_recent,
                    r.calls,
                    r.layers
                );
                out.push_str(&format!(
                    "-- код профиля (token={PROFILE_TOKEN}, лежит в слое long) --\n{}\n{}\n{}\n\n",
                    r.window_profile.line(),
                    r.memory_profile.line(),
                    r.without_long_profile.line()
                ));
                out.push_str(&format!(
                    "-- код задачи (token={TASK_TOKEN}, лежит в слое working) --\n{}\n{}\n{}\n\n",
                    r.window_task.line(),
                    r.memory_task.line(),
                    r.without_long_task.line()
                ));
                out.push_str(&format!(
                    "prompt_tokens: окно {} | память {} | память без long {}\n",
                    r.window_profile.call.prompt_tokens,
                    r.memory_profile.call.prompt_tokens,
                    r.without_long_profile.call.prompt_tokens
                ));
                out.push_str(&format!("=> {}\n", r.verdict.as_str()));
                out.push_str(
                    "Confirmed требует адресной подписи: окно без памяти потеряло ОБА кода, память вернула ОБА, \
а снятие одного только долговременного блока убирает код профиля и оставляет код задачи. Между вторым и третьим прогоном \
отличается ровно один блок в system, поэтому приписать разницу больше нечему. Leaky — окно не резало.\n",
                );
                out
            }
            MemoryReport::Isolation(r) => {
                let mut out = format!(
                    "== модель памяти: разделение слоёв (смена задачи) ==\nмодель: просили {}, ответила {}\nживых вызовов {}\nзадача A = `{}`, задача B = `{}`\n\n",
                    r.asked_model,
                    model_or_q(&r.model),
                    r.calls,
                    r.task_a,
                    r.task_b
                );
                out.push_str(&format!(
                    "-- задача A (рабочий слой A + долговременный) --\n{}\n{}\n\n",
                    r.a_profile.line(),
                    r.a_task.line()
                ));
                out.push_str(&format!(
                    "-- задача B (рабочий слой B, пустой + тот же долговременный) --\n{}\n{}\n\n",
                    r.b_profile.line(),
                    r.b_task.line()
                ));
                out.push_str(&format!(
                    "файл рабочего слоя A: {}\n  код задачи всё ещё в файле: {}\n",
                    r.a_file, r.a_file_keeps_token
                ));
                out.push_str(&format!("=> {}\n", r.verdict.as_str()));
                out.push_str(
                    "Confirmed требует асимметрии: после смены задачи долговременный код всё ещё помнится, рабочий — честно ЗАБЫТ, \
и при этом он не потерян, а лежит в файле своей задачи. Leaky — рабочая память чужой задачи доехала до провода.\n",
                );
                out
            }
        }
    }
}

/// `--verify-memory`: запускает выбранные проверки последовательно.
pub fn run_memory(
    checks: &[MemoryCheck],
    model: Option<&str>,
    offline: bool,
) -> Res<Vec<MemoryReport>> {
    let mut out = Vec::new();
    for check in checks {
        out.push(match check {
            MemoryCheck::Routing => MemoryReport::Routing(run_routing(model, offline)?),
            MemoryCheck::Influence => MemoryReport::Influence(Box::new(run_influence(model)?)),
            MemoryCheck::Isolation => MemoryReport::Isolation(Box::new(run_mem_isolation(model)?)),
        });
    }
    Ok(out)
}

/// Настройки проверок памяти: как у проверок стратегий, но с
/// `temperature=0`.
///
/// Здесь шесть-семь живых вопросов подряд, и каждый — «назови код или
/// NONE». Свободная сэмплировка добавляет к этому шум, который к памяти
/// отношения не имеет: модель иногда выдаёт единственный оставшийся код на
/// любой вопрос. Ноль не смягчает критерий (подпись всё та же), он убирает
/// разброс, который иначе выдаёт честный, но случайный `Flat`.
fn memory_settings(model: Option<&str>) -> Res<Settings> {
    let mut settings = context_settings(model)?;
    settings.temperature = Some(0.0);
    Ok(settings)
}

/// Временный корень памяти: три папки, которые никому больше не мешают.
fn memory_scratch(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "ask-verify-mem-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&path);
    path
}

/// Проверка маршрутизации.
///
/// Первая половина — без сети: фиксированный набор записей (включая одну с
/// нарочно неверной подсказкой слоя и одну вообще без подсказки) проходит
/// через тот же `MemoryStore`, что и приложение, после чего файлы слоёв
/// перечитываются с диска. Проверяется не намерение, а то, что лежит в
/// файле.
///
/// Вторая половина — живая: настоящий экстрактор на коротком разговоре, где
/// посажены личный код и код задачи. Confirmed требует, чтобы он положил их
/// в разные слои, и именно в те.
pub fn run_routing(model: Option<&str>, offline: bool) -> Res<RoutingReport> {
    let root = memory_scratch("routing");
    let mut store = MemoryStore::open(&root, "verify-session", "verify-task");

    // Первая запись приходит с подсказкой `long`, но ключ размечен как
    // рабочий: детерминированное правило обязано победить модель.
    let ops = vec![
        memory::Op::Upsert {
            key: "профиль.язык".into(),
            value: "русский".into(),
            layer: Some(Layer::Long),
        },
        memory::Op::Upsert {
            key: "задача.срок".into(),
            value: "две недели".into(),
            layer: Some(Layer::Long),
        },
        memory::Op::Upsert {
            key: "тема.сейчас".into(),
            value: "обсуждаем модель памяти".into(),
            layer: Some(Layer::Short),
        },
        memory::Op::Upsert {
            key: "решение.хранилище".into(),
            value: "три папки, по файлу на слой".into(),
            layer: None,
        },
        memory::Op::Upsert {
            key: "бюджет".into(),
            value: "не обсуждали".into(),
            layer: None,
        },
    ];
    let expected = [
        ("профиль.язык", Layer::Long),
        ("задача.срок", Layer::Working),
        ("тема.сейчас", Layer::Short),
        ("решение.хранилище", Layer::Long),
        // Ни префикса, ни подсказки — по умолчанию рабочий слой, а не
        // долговременный профиль.
        ("бюджет", Layer::Working),
    ];
    let delta = store.apply_ops(&ops);
    let mut cases = Vec::new();
    for (key, want) in expected {
        let (landed, reason) = delta
            .routes
            .iter()
            .find(|(_, k, _)| k == key)
            .map(|(l, _, r)| (*l, r.clone()))
            .unwrap_or((Layer::Working, "не размечено".into()));
        let asked = ops
            .iter()
            .find_map(|op| match op {
                memory::Op::Upsert { key: k, layer, .. } if k == key => {
                    Some(layer.map(|l| l.label().to_string()).unwrap_or("-".into()))
                }
                _ => None,
            })
            .unwrap_or_else(|| "-".into());
        cases.push(RouteCase {
            key: key.to_string(),
            asked,
            landed,
            expected: want,
            reason,
        });
    }

    // Перечитываем файлы с диска новым стором: проверяем хранилище, а не
    // оперативку.
    let reread = MemoryStore::open(&root, "verify-session", "verify-task");
    let files: Vec<LayerFileView> = Layer::ALL
        .iter()
        .map(|layer| LayerFileView {
            layer: *layer,
            path: reread.path(*layer).display().to_string(),
            keys: reread
                .records(*layer)
                .iter()
                .map(|r| r.key.clone())
                .collect(),
        })
        .collect();
    let on_disk_ok = expected.iter().all(|(key, want)| {
        Layer::ALL.iter().all(|layer| {
            let here = reread.get(*layer, key).is_some();
            here == (*layer == *want)
        })
    });

    let live = if offline {
        None
    } else {
        Some(live_routing(model, &root)?)
    };
    let verdict = judge_routing(&cases, on_disk_ok, live.as_ref());
    let _ = fs::remove_dir_all(&root);
    Ok(RoutingReport {
        cases,
        files,
        live,
        verdict,
    })
}

/// Живая половина маршрутизации: экстрактор гоняется ровно так, как это
/// делает TUI, — после каждой реплики пользователя.
fn live_routing(model: Option<&str>, root: &Path) -> Res<LiveRouting> {
    let mut settings = memory_settings(model)?;
    settings.keep_recent = MEMORY_KEEP;
    settings.context_strategy = ContextStrategy::Memory;
    let asked_model = settings.model.clone();
    let mut agent = Agent::new(settings)?;
    agent.set_context_enabled(false);
    agent.set_memory(MemoryStore::open(root, "verify-live", "verify-live"));

    let dialogue = memory_dialogue();
    let mut calls = 0usize;
    let mut errors = Vec::new();
    let mut routes: Vec<(Layer, String, String)> = Vec::new();
    for i in 0..dialogue.len() {
        if dialogue[i].role != crate::api::Role::User {
            continue;
        }
        calls += 1;
        match agent.update_memory(&dialogue[..=i]) {
            Ok(delta) => routes.extend(delta.routes),
            Err(e) => errors.push(e),
        }
    }
    let profile_layer = layer_holding(agent.memory(), PROFILE_TOKEN);
    let task_layer = layer_holding(agent.memory(), TASK_TOKEN);
    Ok(LiveRouting {
        asked_model,
        calls,
        errors,
        routes,
        profile_layer,
        task_layer,
    })
}

/// В каком слое лежит значение с этим токеном.
fn layer_holding(store: &MemoryStore, token: &str) -> Option<Layer> {
    Layer::ALL.iter().copied().find(|layer| {
        store
            .records(*layer)
            .iter()
            .any(|r| has_token(&r.value, token))
    })
}

/// Причинная подпись маршрутизации: файл, а не намерение.
///
/// * Хоть одна запись легла не в свой слой → `Leaky`.
/// * На диске запись видна не в том файле (или видна в двух) → `Leaky`.
/// * Живой экстрактор не положил посаженные коды в разные ожидаемые слои →
///   `Inconclusive`: маршрутизатор-то прав, а вот доказать живой путь нечем.
fn judge_routing(
    cases: &[RouteCase],
    on_disk_ok: bool,
    live: Option<&LiveRouting>,
) -> ContextVerdict {
    if !cases.iter().all(RouteCase::ok) || !on_disk_ok {
        return ContextVerdict::Leaky;
    }
    match live {
        None => ContextVerdict::Confirmed,
        Some(l) => {
            if l.profile_layer == Some(Layer::Long) && l.task_layer == Some(Layer::Working) {
                ContextVerdict::Confirmed
            } else {
                ContextVerdict::Inconclusive
            }
        }
    }
}

/// Проверка влияния слоёв на ответ.
///
/// Память набивается вручную — ровно так, как её набивает человек через
/// `/mem long set ...`: проверка про влияние слоя на ответ не должна падать
/// из-за того, что экстрактор в этот раз выбрал другой ключ (за него
/// отвечает `routing`). Дальше один и тот же разговор и одни и те же два
/// вопроса задаются трижды, и между вторым и третьим прогоном отличается
/// ровно один блок в system.
pub fn run_influence(model: Option<&str>) -> Res<InfluenceReport> {
    let root = memory_scratch("influence");
    let mut settings = memory_settings(model)?;
    settings.keep_recent = MEMORY_KEEP;
    settings.context_strategy = ContextStrategy::Window;
    let asked_model = settings.model.clone();
    let mut agent = Agent::new(settings)?;
    agent.set_context_enabled(false);

    let mut store = MemoryStore::open(&root, "influence", "стенд");
    store.set(Layer::Long, PROFILE_KEY, PROFILE_TOKEN);
    store.set(Layer::Working, TASK_KEY, TASK_TOKEN);
    store.set(Layer::Short, "тема.сейчас", "проверяем модель памяти");
    let layers = store.status_line();
    agent.set_memory(store);

    let dialogue = memory_dialogue();
    // 1. Окно без блоков памяти: оба кода уехали за границу окна.
    let window_profile = ContextProbe {
        label: "window (без памяти)".into(),
        token: PROFILE_TOKEN,
        call: ask_call(&agent, &dialogue, PROFILE_QUESTION)?,
    };
    let window_task = ContextProbe {
        label: "window (без памяти)".into(),
        token: TASK_TOKEN,
        call: ask_call(&agent, &dialogue, TASK_QUESTION)?,
    };

    // 2. То же окно плюс три блока памяти в system.
    agent.set_strategy(ContextStrategy::Memory);
    let memory_profile = ContextProbe {
        label: "memory (три слоя)".into(),
        token: PROFILE_TOKEN,
        call: ask_call(&agent, &dialogue, PROFILE_QUESTION)?,
    };
    let memory_task = ContextProbe {
        label: "memory (три слоя)".into(),
        token: TASK_TOKEN,
        call: ask_call(&agent, &dialogue, TASK_QUESTION)?,
    };

    // 3. Снимаем ровно один блок — долговременный.
    agent.memory_mut().clear(Layer::Long);
    let without_long_profile = ContextProbe {
        label: "memory без long".into(),
        token: PROFILE_TOKEN,
        call: ask_call(&agent, &dialogue, PROFILE_QUESTION)?,
    };
    let without_long_task = ContextProbe {
        label: "memory без long".into(),
        token: TASK_TOKEN,
        call: ask_call(&agent, &dialogue, TASK_QUESTION)?,
    };

    let verdict = judge_influence(
        &window_profile,
        &window_task,
        &memory_profile,
        &memory_task,
        &without_long_profile,
        &without_long_task,
    );
    let _ = fs::remove_dir_all(&root);
    Ok(InfluenceReport {
        model: memory_profile.call.model.clone(),
        asked_model,
        calls: 6,
        messages: dialogue.len(),
        keep_recent: MEMORY_KEEP,
        layers,
        window_profile,
        window_task,
        memory_profile,
        memory_task,
        without_long_profile,
        without_long_task,
        verdict,
    })
}

/// Причинная подпись влияния слоёв.
///
/// * Окно само помнит хоть один код → `Leaky`: оно не резало, и дальше
///   доказывать нечего.
/// * Память не вернула оба кода → `Inconclusive`.
/// * Снятие долговременного блока не убрало код профиля → `Leaky`: ответ
///   брался не из этого слоя.
/// * Снятие долговременного блока убило и код задачи → `Flat`: слои не
///   различимы по влиянию, а значит «разделены» — только на словах.
fn judge_influence(
    window_profile: &ContextProbe,
    window_task: &ContextProbe,
    memory_profile: &ContextProbe,
    memory_task: &ContextProbe,
    without_long_profile: &ContextProbe,
    without_long_task: &ContextProbe,
) -> ContextVerdict {
    if window_profile.recalled() || window_task.recalled() {
        return ContextVerdict::Leaky;
    }
    if !memory_profile.recalled() || !memory_task.recalled() {
        return ContextVerdict::Inconclusive;
    }
    if without_long_profile.recalled() {
        return ContextVerdict::Leaky;
    }
    if !without_long_task.recalled() {
        return ContextVerdict::Flat;
    }
    ContextVerdict::Confirmed
}

/// Проверка разделения слоёв на смене задачи.
///
/// Вопросы задаются без истории вообще: на проводе только блоки памяти,
/// поэтому ответ может прийти только из них. Сначала задача A (рабочая
/// память с кодом), потом та же память при задаче B.
pub fn run_mem_isolation(model: Option<&str>) -> Res<MemIsolationReport> {
    let root = memory_scratch("isolation");
    let mut settings = memory_settings(model)?;
    settings.keep_recent = MEMORY_KEEP;
    settings.context_strategy = ContextStrategy::Memory;
    let asked_model = settings.model.clone();
    let mut agent = Agent::new(settings)?;
    agent.set_context_enabled(false);

    let task_a = "стенд-альфа";
    let task_b = "отчёт-бета";
    let mut store = MemoryStore::open(&root, "isolation", task_a);
    store.set(Layer::Long, PROFILE_KEY, PROFILE_TOKEN);
    store.set(Layer::Working, TASK_KEY, TASK_TOKEN);
    let a_file = store.path(Layer::Working).display().to_string();
    agent.set_memory(store);

    let empty: Vec<ChatMessage> = Vec::new();
    let a_profile = ContextProbe {
        label: format!("задача `{task_a}`"),
        token: PROFILE_TOKEN,
        call: ask_call(&agent, &empty, PROFILE_QUESTION)?,
    };
    let a_task = ContextProbe {
        label: format!("задача `{task_a}`"),
        token: TASK_TOKEN,
        call: ask_call(&agent, &empty, TASK_QUESTION)?,
    };

    // Смена задачи: рабочий слой уезжает в свой файл, на его место встаёт
    // пустой слой задачи B. Долговременный слой не трогается.
    agent.memory_mut().set_task(task_b);
    let b_profile = ContextProbe {
        label: format!("задача `{task_b}`"),
        token: PROFILE_TOKEN,
        call: ask_call(&agent, &empty, PROFILE_QUESTION)?,
    };
    let b_task = ContextProbe {
        label: format!("задача `{task_b}`"),
        token: TASK_TOKEN,
        call: ask_call(&agent, &empty, TASK_QUESTION)?,
    };

    let a_file_keeps_token = fs::read_to_string(&a_file)
        .map(|s| s.contains(TASK_TOKEN))
        .unwrap_or(false);
    let verdict = judge_mem_isolation(&a_profile, &a_task, &b_profile, &b_task, a_file_keeps_token);
    let report = MemIsolationReport {
        model: a_profile.call.model.clone(),
        asked_model,
        calls: 4,
        task_a: task_a.into(),
        task_b: task_b.into(),
        a_profile,
        a_task,
        b_profile,
        b_task,
        a_file,
        a_file_keeps_token,
        verdict,
    };
    let _ = fs::remove_dir_all(&root);
    Ok(report)
}

/// Причинная подпись разделения слоёв.
///
/// * Задача A не знала своих кодов → `Inconclusive`: сравнивать не с чем.
/// * После смены задачи рабочий код всё ещё помнится → `Leaky`: рабочие
///   слои не разделены.
/// * Долговременный код после смены задачи потерялся → `Flat`: слои живут
///   одной жизнью, значит слоёв на самом деле нет.
/// * Рабочий код исчез с провода, но пропал и из файла своей задачи →
///   `Inconclusive`: это уже не разделение, а потеря данных.
fn judge_mem_isolation(
    a_profile: &ContextProbe,
    a_task: &ContextProbe,
    b_profile: &ContextProbe,
    b_task: &ContextProbe,
    a_file_keeps_token: bool,
) -> ContextVerdict {
    if !a_profile.recalled() || !a_task.recalled() {
        return ContextVerdict::Inconclusive;
    }
    if b_task.recalled() {
        return ContextVerdict::Leaky;
    }
    if !b_profile.recalled() {
        return ContextVerdict::Flat;
    }
    if !a_file_keeps_token {
        return ContextVerdict::Inconclusive;
    }
    ContextVerdict::Confirmed
}

/// Разговор для проверок памяти: 12 сообщений, два кода посажены в самом
/// начале — при `keep_recent=4` оба уезжают за границу окна, и ответить на
/// вопрос о них можно только из памяти.
pub fn memory_dialogue() -> Vec<ChatMessage> {
    let topics = [
        (
            "Задача: собрать нагрузочный стенд сервиса заказов за две недели.",
            "Понял: стенд для сервиса заказов, срок две недели.",
        ),
        (
            "Стек: Rust, PostgreSQL 16, очередь NATS, всё в Kubernetes.",
            "Записал стек: Rust, PostgreSQL 16, NATS, Kubernetes.",
        ),
        (
            "Целевая нагрузка 4000 запросов в секунду, 85 процентов чтение.",
            "Цель 4000 rps с перекосом 85/15 зафиксирована.",
        ),
        (
            "Отчёт нужен в понедельник утром: таблица сценариев и вывод.",
            "Отчёт к понедельнику: таблица по сценариям и приоритет починки.",
        ),
    ];
    let mut history = vec![
        ChatMessage::user(format!(
            "Запомни про меня навсегда: мой личный код — {PROFILE_TOKEN}. Он не про текущую задачу, он про меня."
        )),
        ChatMessage::assistant(format!("Запомнил: ваш личный код {PROFILE_TOKEN}.")),
        ChatMessage::user(format!(
            "А код текущей задачи — {TASK_TOKEN}. Он живёт только пока мы делаем эту задачу."
        )),
        ChatMessage::assistant(format!("Записал код задачи {TASK_TOKEN}.")),
    ];
    for (q, a) in topics {
        history.push(ChatMessage::user(q.to_string()));
        history.push(ChatMessage::assistant(a.to_string()));
    }
    history
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compress::{Compressor, Policy};
    use crate::config::Effort;

    /// Офлайн-половина проверки памяти — настоящая: она гоняет тот же
    /// `MemoryStore`, что и приложение, и читает файлы слоёв с диска.
    #[test]
    fn routing_check_runs_without_a_network_and_confirms() {
        let report = run_routing(None, true).expect("офлайн-маршрутизация не должна ходить в сеть");
        assert_eq!(report.verdict, ContextVerdict::Confirmed);
        assert_eq!(report.calls_live(), 0);
        assert!(report.live.is_none());
        // Каждый ключ виден ровно в файле своего слоя.
        let long = report
            .files
            .iter()
            .find(|f| f.layer == Layer::Long)
            .unwrap();
        assert!(long.keys.contains(&"профиль.язык".to_string()));
        assert!(!long.keys.contains(&"задача.срок".to_string()));
        let rendered = MemoryReport::Routing(report).render();
        assert!(!rendered.contains("НЕ ТУДА"), "{rendered}");
    }

    /// Токены проверок памяти обязаны уезжать за границу окна — иначе
    /// «память вернула факт» ничего не доказывает.
    #[test]
    fn memory_tokens_land_outside_the_window() {
        let dialogue = memory_dialogue();
        let wire = crate::strategy::window(&dialogue, MEMORY_KEEP);
        let tail: String = wire.iter().map(|m| m.content.clone()).collect();
        assert!(!tail.contains(PROFILE_TOKEN), "код профиля остался в окне");
        assert!(!tail.contains(TASK_TOKEN), "код задачи остался в окне");
    }

    fn mem_probe(token: &'static str, text: &str) -> ContextProbe {
        ContextProbe {
            label: "t".into(),
            token,
            call: call("stop", 5, text),
        }
    }

    #[test]
    fn influence_is_confirmed_only_when_one_block_explains_the_difference() {
        let forgot_p = mem_probe(PROFILE_TOKEN, "NONE");
        let forgot_t = mem_probe(TASK_TOKEN, "NONE");
        let knows_p = mem_probe(PROFILE_TOKEN, PROFILE_TOKEN);
        let knows_t = mem_probe(TASK_TOKEN, TASK_TOKEN);
        // Снятие long убрало код профиля и оставило код задачи — подпись.
        assert_eq!(
            judge_influence(&forgot_p, &forgot_t, &knows_p, &knows_t, &forgot_p, &knows_t),
            ContextVerdict::Confirmed
        );
        // Окно само всё помнит — доказывать нечего.
        assert_eq!(
            judge_influence(&knows_p, &forgot_t, &knows_p, &knows_t, &forgot_p, &knows_t),
            ContextVerdict::Leaky
        );
        // Слой сняли, а ответ не изменился — ответ брался не из него.
        assert_eq!(
            judge_influence(&forgot_p, &forgot_t, &knows_p, &knows_t, &knows_p, &knows_t),
            ContextVerdict::Leaky
        );
        // Снятие одного слоя убило и чужой код — слои неразличимы.
        assert_eq!(
            judge_influence(&forgot_p, &forgot_t, &knows_p, &knows_t, &forgot_p, &forgot_t),
            ContextVerdict::Flat
        );
        // Память ничего не вернула.
        assert_eq!(
            judge_influence(&forgot_p, &forgot_t, &forgot_p, &knows_t, &forgot_p, &knows_t),
            ContextVerdict::Inconclusive
        );
    }

    #[test]
    fn isolation_is_confirmed_only_when_the_working_layer_is_forgotten_but_not_lost() {
        let knows_p = mem_probe(PROFILE_TOKEN, PROFILE_TOKEN);
        let knows_t = mem_probe(TASK_TOKEN, TASK_TOKEN);
        let forgot_p = mem_probe(PROFILE_TOKEN, "NONE");
        let forgot_t = mem_probe(TASK_TOKEN, "NONE");
        assert_eq!(
            judge_mem_isolation(&knows_p, &knows_t, &knows_p, &forgot_t, true),
            ContextVerdict::Confirmed
        );
        // Рабочая память чужой задачи доехала до провода.
        assert_eq!(
            judge_mem_isolation(&knows_p, &knows_t, &knows_p, &knows_t, true),
            ContextVerdict::Leaky
        );
        // Смена задачи снесла и долговременный слой — слоёв нет.
        assert_eq!(
            judge_mem_isolation(&knows_p, &knows_t, &forgot_p, &forgot_t, true),
            ContextVerdict::Flat
        );
        // Забыли и потеряли — это не разделение.
        assert_eq!(
            judge_mem_isolation(&knows_p, &knows_t, &knows_p, &forgot_t, false),
            ContextVerdict::Inconclusive
        );
    }

    #[test]
    fn memory_checks_parse_their_names() {
        assert_eq!(MemoryCheck::parse("all").unwrap(), MemoryCheck::ALL.to_vec());
        assert_eq!(
            MemoryCheck::parse("routing").unwrap(),
            vec![MemoryCheck::Routing]
        );
        assert!(MemoryCheck::parse("что-то").is_err());
    }

    fn probe(
        token: &'static str,
        full_prompt: u64,
        comp_prompt: u64,
        full_text: &str,
        comp_text: &str,
    ) -> CompressionProbe {
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
        let policy = Policy {
            keep_recent: COMPRESS_KEEP_RECENT,
            every: COMPRESS_EVERY,
        };
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
        assert_eq!(
            judge_compression(&folded, &tail),
            CompressionVerdict::Confirmed
        );
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
        assert_eq!(judge_instruction("42", "42", "QUINCE"), LeverVerdict::Flat);
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
    fn has_word_is_word_bounded_on_cyrillic() {
        assert!(has_word("Евгений, здравствуй!", "Евгений"));
        assert!(has_word("привет, евгений", "Евгений"));
        assert!(!has_word("Евгения нет", "Евгений"));
        assert!(!has_word("Коротко: 144", "Евгений"));
    }

    #[test]
    fn profile_checks_parse_their_names() {
        assert_eq!(ProfileCheck::parse("all").unwrap(), ProfileCheck::ALL.to_vec());
        assert_eq!(
            ProfileCheck::parse("voice").unwrap(),
            vec![ProfileCheck::Voice]
        );
        assert!(ProfileCheck::parse("голос").is_err());
    }

    /// Проводная половина проверки персонализации — офлайновая, поэтому она
    /// же и тест: блок ровно один, пункты профиля доехали, при смене
    /// профиля всё остальное в `system` не шелохнулось.
    #[test]
    fn wire_check_confirms_without_a_network() {
        let report = run_profile_wire().unwrap();
        let status = ProfileReport::Wire(report.clone()).status_line();
        assert_eq!(report.verdict, ContextVerdict::Confirmed, "{status}");
        assert!(report.rest_identical && report.blocks_differ && report.memory_block_kept);
        assert!(report.cases.iter().all(|c| c.missing.is_empty() && c.blocks <= 1));
        // Первый случай — профиль выключен: блока нет, дефолтный промпт цел.
        assert!(report.cases[0].block.is_none() && report.cases[0].default_prompt);
    }

    /// Матрица подписей: «тексты разные» — не подтверждение. Confirmed
    /// только когда каждый ответ несёт свою подпись и не несёт чужую.
    #[test]
    fn voice_verdict_needs_the_cross_matrix_not_a_text_diff() {
        let chem = profile::builtins()
            .into_iter()
            .find(|p| p.id == "chemist")
            .unwrap()
            .marker
            .unwrap();
        let gop = profile::builtins()
            .into_iter()
            .find(|p| p.id == "gopnik")
            .unwrap()
            .marker
            .unwrap();
        // Два разных, но одинаково безликих ответа — ни одной подписи.
        let flat_a = "Свет рассеивается на молекулах воздуха.";
        let flat_b = "Молекулы воздуха рассеивают короткие волны сильнее.";
        assert_ne!(flat_a, flat_b);
        assert!(!chem.matches(flat_a) && !gop.matches(flat_b));
        // А так — сошлось: своя подпись есть, чужой нет.
        assert!(chem.matches("Гипотеза: рэлеевское рассеяние. Вывод: да."));
        assert!(gop.matches("Братан, короче, воздух свет раскидывает."));
    }

    #[test]
    fn endpoint_is_the_plain_z_ai_completions_url() {
        let url = completions_url();
        assert_eq!(url, format!("{DEFAULT_BASE_URL}/chat/completions"));
        assert!(!url.contains("/coding/"));
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Персонализация: профиль пользователя (`profile.rs`)
// ═══════════════════════════════════════════════════════════════════════

/// Какую половину персонализации проверяем.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProfileCheck {
    /// Что именно уезжает в `system`: блок профиля есть, он один, и при
    /// смене профиля меняется **только** он. Без сети.
    Wire,
    /// Живьём: один и тот же вопрос в двух профилях. Confirmed только когда
    /// каждый ответ несёт подпись своего профиля и не несёт чужую.
    Voice,
    /// Живьём: то, что агент учитывает автоматически — записи `профиль.*`
    /// из долговременной памяти внутри блока профиля.
    Auto,
}

impl ProfileCheck {
    pub const ALL: [ProfileCheck; 3] = [ProfileCheck::Wire, ProfileCheck::Voice, ProfileCheck::Auto];

    pub fn label(self) -> &'static str {
        match self {
            ProfileCheck::Wire => "wire",
            ProfileCheck::Voice => "voice",
            ProfileCheck::Auto => "auto",
        }
    }

    /// `wire|voice|auto|all`.
    pub fn parse(s: &str) -> Res<Vec<ProfileCheck>> {
        match s.trim().to_ascii_lowercase().as_str() {
            "all" | "" => Ok(ProfileCheck::ALL.to_vec()),
            "wire" | "prompt" | "system" => Ok(vec![ProfileCheck::Wire]),
            "voice" | "answers" | "style" => Ok(vec![ProfileCheck::Voice]),
            "auto" | "memory" | "known" => Ok(vec![ProfileCheck::Auto]),
            other => Err(format!(
                "unknown profile check `{other}`; expected wire, voice, auto or all"
            )),
        }
    }
}

/// Один собранный `system`: что в нём от профиля и что осталось вокруг.
#[derive(Clone, Debug)]
pub struct WireCase {
    pub label: String,
    pub profile: String,
    /// Блок профиля, если он есть.
    pub block: Option<String>,
    /// Сколько раз встретился заголовок блока: два блока — это баг.
    pub blocks: usize,
    /// Всё, что не блок профиля.
    pub rest: String,
    /// Остался ли в `system` дефолтный «ты — полезный ассистент».
    pub default_prompt: bool,
    /// Пункты профиля, не доехавшие до блока (должен быть пуст).
    pub missing: Vec<String>,
}

impl WireCase {
    fn line(&self) -> String {
        format!(
            "[{}] профиль={} блоков={} дефолтный промпт={} потеряно пунктов={} остальное={} симв.",
            self.label,
            self.profile,
            self.blocks,
            self.default_prompt,
            self.missing.len(),
            self.rest.chars().count()
        )
    }
}

#[derive(Clone, Debug)]
pub struct ProfileWireReport {
    pub cases: Vec<WireCase>,
    /// Совпало ли «остальное» у двух разных профилей поверх своего промпта.
    pub rest_identical: bool,
    /// Отличаются ли сами блоки.
    pub blocks_differ: bool,
    /// Уцелел ли блок долговременной памяти при смене профиля.
    pub memory_block_kept: bool,
    pub verdict: ContextVerdict,
}

/// Ответ одного профиля на общий вопрос плюс матрица подписей.
#[derive(Clone, Debug)]
pub struct VoiceCase {
    pub profile: String,
    pub title: String,
    pub call: Call,
    /// Своя подпись сошлась.
    pub own: bool,
    /// Чужая подпись сошлась (должна не сходиться).
    pub other: bool,
}

impl VoiceCase {
    fn line(&self) -> String {
        format!(
            "[{}] {} — {}\n  {}\n  своя подпись: {} · чужая подпись: {}",
            self.profile,
            self.title,
            self.call.line(),
            clip(&self.call.text, 200),
            self.own,
            self.other
        )
    }
}

#[derive(Clone, Debug)]
pub struct ProfileVoiceReport {
    /// Какую модель просили. Какая ответила — в `model` каждого вызова.
    pub asked_model: String,
    pub calls: usize,
    pub question: String,
    pub a: VoiceCase,
    pub b: VoiceCase,
    /// Контроль без профиля: тот же вопрос, обычный ассистент.
    pub control: Call,
    pub control_hits_a: bool,
    pub control_hits_b: bool,
    pub verdict: ContextVerdict,
}

#[derive(Clone, Debug)]
pub struct ProfileAutoReport {
    /// Какую модель просили. Какая ответила — в `model` каждого вызова.
    pub asked_model: String,
    pub calls: usize,
    pub profile: String,
    pub question: String,
    pub token: String,
    /// Профиль плюс записи `профиль.*` в долговременной памяти.
    pub with_memory: Call,
    /// Тот же профиль, память про пользователя пуста.
    pub without_memory: Call,
    pub with_recalled: bool,
    pub without_recalled: bool,
    pub verdict: ContextVerdict,
}

#[derive(Clone, Debug)]
pub enum ProfileReport {
    Wire(ProfileWireReport),
    Voice(Box<ProfileVoiceReport>),
    Auto(Box<ProfileAutoReport>),
}

impl ProfileReport {
    pub fn verdict(&self) -> ContextVerdict {
        match self {
            ProfileReport::Wire(r) => r.verdict,
            ProfileReport::Voice(r) => r.verdict,
            ProfileReport::Auto(r) => r.verdict,
        }
    }

    pub fn confirmed(&self) -> bool {
        self.verdict() == ContextVerdict::Confirmed
    }

    pub fn calls(&self) -> usize {
        match self {
            ProfileReport::Wire(_) => 0,
            ProfileReport::Voice(r) => r.calls,
            ProfileReport::Auto(r) => r.calls,
        }
    }

    pub fn status_line(&self) -> String {
        match self {
            ProfileReport::Wire(r) => format!(
                "wire: verdict={} случаев={} остальное не тронуто={} блоки различаются={} блок памяти цел={}",
                r.verdict.as_str(),
                r.cases.len(),
                r.rest_identical,
                r.blocks_differ,
                r.memory_block_kept
            ),
            ProfileReport::Voice(r) => format!(
                "voice: verdict={} {}(своя={} чужая={}) {}(своя={} чужая={})",
                r.verdict.as_str(),
                r.a.profile,
                r.a.own,
                r.a.other,
                r.b.profile,
                r.b.own,
                r.b.other
            ),
            ProfileReport::Auto(r) => format!(
                "auto: verdict={} профиль={} с памятью помнит {}={} без памяти={} prompt_tokens {}→{}",
                r.verdict.as_str(),
                r.profile,
                r.token,
                r.with_recalled,
                r.without_recalled,
                r.without_memory.prompt_tokens,
                r.with_memory.prompt_tokens
            ),
        }
    }

    pub fn render(&self) -> String {
        match self {
            ProfileReport::Wire(r) => {
                let mut out = String::from(
                    "=== профиль на проводе (без сети) ===\n\
                     один и тот же запрос, меняется только настройка `profile`\n\n",
                );
                for c in &r.cases {
                    out.push_str(&c.line());
                    out.push('\n');
                }
                out.push_str(&format!(
                    "\nодин профиль — один блок: {}\n\
                     профиль заменяет дефолтный промпт: {}\n\
                     свой системный промпт цел: {}\n\
                     при смене профиля остальное в system не изменилось: {}\n\
                     блоки двух профилей различаются: {}\n\
                     блок долговременной памяти на месте: {}\n\
                     verdict={}\n",
                    r.cases.iter().all(|c| c.blocks <= 1),
                    r.cases
                        .iter()
                        .filter(|c| c.profile != profile::OFF)
                        .all(|c| !c.default_prompt),
                    r.cases
                        .iter()
                        .filter(|c| c.label.starts_with("свой промпт"))
                        .all(|c| c.rest.contains(CUSTOM_PROMPT)),
                    r.rest_identical,
                    r.blocks_differ,
                    r.memory_block_kept,
                    r.verdict.as_str()
                ));
                out
            }
            ProfileReport::Voice(r) => {
                let mut out = format!(
                    "=== голоса двух профилей (живьём) ===\nмодель: просили {} \n\
                     вопрос (один и тот же): {}\n\n",
                    r.asked_model, r.question
                );
                out.push_str(&r.a.line());
                out.push_str("\n\n");
                out.push_str(&r.b.line());
                out.push_str(&format!(
                    "\n\n[контроль без профиля] {}\n  {}\n  подпись {}: {} · подпись {}: {}\n",
                    r.control.line(),
                    clip(&r.control.text, 200),
                    r.a.profile,
                    r.control_hits_a,
                    r.b.profile,
                    r.control_hits_b
                ));
                out.push_str(&format!(
                    "\nкаждый ответ несёт свою подпись и не несёт чужую → verdict={}\n",
                    r.verdict.as_str()
                ));
                out
            }
            ProfileReport::Auto(r) => {
                let mut out = format!(
                    "=== что профиль учитывает автоматически (живьём) ===\n\
                     модель: просили {}\nпрофиль: {}\n\
                     вопрос (про имя не спрашиваем): {}\n\n",
                    r.asked_model, r.profile, r.question
                );
                out.push_str(&format!(
                    "[профиль + записи профиль.* в долговременной памяти] {}\n  {}\n  помнит {}: {}\n\n",
                    r.with_memory.line(),
                    clip(&r.with_memory.text, 200),
                    r.token,
                    r.with_recalled
                ));
                out.push_str(&format!(
                    "[тот же профиль, память про пользователя пуста] {}\n  {}\n  помнит {}: {}\n\n",
                    r.without_memory.line(),
                    clip(&r.without_memory.text, 200),
                    r.token,
                    r.without_recalled
                ));
                out.push_str(&format!(
                    "разница ровно в двух строках блока профиля: prompt_tokens {} → {}\nverdict={}\n",
                    r.without_memory.prompt_tokens,
                    r.with_memory.prompt_tokens,
                    r.verdict.as_str()
                ));
                out
            }
        }
    }
}

/// Свой системный промпт для `wire`-случая: он не должен пострадать от
/// профиля.
const CUSTOM_PROMPT: &str = "Отвечай только проверяемыми фактами.";

pub fn run_profile(checks: &[ProfileCheck], model: Option<&str>) -> Res<Vec<ProfileReport>> {
    let mut out = Vec::new();
    for check in checks {
        out.push(match check {
            ProfileCheck::Wire => ProfileReport::Wire(run_profile_wire()?),
            ProfileCheck::Voice => ProfileReport::Voice(Box::new(run_profile_voice(model)?)),
            ProfileCheck::Auto => ProfileReport::Auto(Box::new(run_profile_auto(model)?)),
        });
    }
    Ok(out)
}

/// Агент без сети: эндпойнт заведомо нерабочий, но `system_for_request`
/// собирается тем же кодом, что и в настоящем запросе.
fn wire_agent(system_prompt: &str, profile_id: &str) -> Res<Agent> {
    let settings = Settings {
        system_prompt: system_prompt.to_string(),
        context_enabled: false,
        ..Settings::default()
    };
    let mut agent = Agent::with_endpoint(api::Endpoint::unusable(), settings);
    agent.set_profile(profile_id)?;
    Ok(agent)
}

/// Разобрать собранный `system` на блок профиля и всё остальное, заодно
/// пересчитав, что из профиля до блока не доехало.
fn wire_case(label: &str, agent: &Agent) -> WireCase {
    let system = agent.system_for_request();
    let (block, rest) = profile::split_block(&system);
    let blocks = system.matches(profile::BLOCK_HEAD).count();
    let missing = match agent.profile() {
        Some(p) => {
            let body = block.clone().unwrap_or_default();
            let mut want: Vec<String> = vec![p.style.clone(), p.format.clone()];
            want.extend(p.limits.iter().cloned());
            want.into_iter()
                .filter(|w| !w.trim().is_empty() && !body.contains(w.trim()))
                .collect()
        }
        None => Vec::new(),
    };
    WireCase {
        label: label.to_string(),
        profile: agent.settings().profile.clone(),
        block,
        blocks,
        default_prompt: system.contains(config::DEFAULT_SYSTEM_PROMPT),
        rest,
        missing,
    }
}

/// Что реально уезжает в `system`. Ноль запросов: проверяется сборка, а
/// сборку видно целиком.
pub fn run_profile_wire() -> Res<ProfileWireReport> {
    let mut cases = Vec::new();

    // 1. Дефолтный системный промпт: профиль его заменяет.
    let off = wire_agent(config::DEFAULT_SYSTEM_PROMPT, profile::OFF)?;
    cases.push(wire_case("дефолтный промпт", &off));
    let chem = wire_agent(config::DEFAULT_SYSTEM_PROMPT, "chemist")?;
    cases.push(wire_case("дефолтный промпт", &chem));

    // 2. Свой системный промпт: профиль его не трогает, они складываются.
    let a = wire_agent(CUSTOM_PROMPT, "chemist")?;
    cases.push(wire_case("свой промпт", &a));
    let b = wire_agent(CUSTOM_PROMPT, "gopnik")?;
    cases.push(wire_case("свой промпт", &b));
    let rest_identical = cases[2].rest == cases[3].rest && cases[2].rest.contains(CUSTOM_PROMPT);
    let blocks_differ = cases[2].block != cases[3].block;

    // 3. Профиль поверх памяти: блок долговременного слоя не должен
    //    пострадать от смены голоса.
    let mut mem_agent = wire_agent(CUSTOM_PROMPT, "gopnik")?;
    mem_agent.set_strategy(ContextStrategy::Memory);
    let mut store = MemoryStore::in_memory();
    store.apply_ops(&[memory::Op::Upsert {
        key: "профиль.имя".into(),
        value: "Евгений".into(),
        layer: Some(Layer::Long),
    }]);
    mem_agent.set_memory(store);
    cases.push(wire_case("память + профиль", &mem_agent));
    let with_memory = &cases[4];
    let memory_block_kept =
        with_memory.rest.contains(Layer::Long.title()) && with_memory.rest.contains("Евгений");

    let verdict = if cases.iter().any(|c| c.blocks > 1 || !c.missing.is_empty())
        || cases[0].block.is_some()
        || !cases[0].default_prompt
        || cases[1].block.is_none()
        || cases[1].default_prompt
        || cases[2].block.is_none()
        || !rest_identical
        || !blocks_differ
        || !memory_block_kept
    {
        ContextVerdict::Inconclusive
    } else {
        ContextVerdict::Confirmed
    };

    Ok(ProfileWireReport {
        cases,
        rest_identical,
        blocks_differ,
        memory_block_kept,
        verdict,
    })
}

/// Настройки живых проверок профиля: та же изоляция, что у проверок памяти,
/// плюс `temperature=0` — разброс сэмплировки к персонализации отношения не
/// имеет.
fn profile_settings(model: Option<&str>) -> Res<Settings> {
    let mut settings = context_settings(model)?;
    settings.temperature = Some(0.0);
    Ok(settings)
}

/// Один вопрос — два голоса. Confirmed только по матрице подписей: свой
/// маркер есть, чужого нет, у обоих. «Тексты разные» здесь не аргумент —
/// два вызова одной модели разойдутся и без всякого профиля.
pub fn run_profile_voice(model: Option<&str>) -> Res<ProfileVoiceReport> {
    let settings = profile_settings(model)?;
    let asked_model = settings.model.clone();
    let question = "Почему небо голубое?";

    let catalog = profile::ProfileSet::in_memory();
    let pa = catalog
        .get("chemist")
        .ok_or("во встроенном каталоге нет профиля chemist")?
        .clone();
    let pb = catalog
        .get("gopnik")
        .ok_or("во встроенном каталоге нет профиля gopnik")?
        .clone();
    let ma = pa.marker.clone().ok_or("у профиля chemist нет подписи")?;
    let mb = pb.marker.clone().ok_or("у профиля gopnik нет подписи")?;

    let mut agent = Agent::new(settings.clone())?;
    agent.set_profile("chemist")?;
    let call_a = probe(&agent, question)?;
    agent.set_profile("gopnik")?;
    let call_b = probe(&agent, question)?;
    agent.set_profile(profile::OFF)?;
    let control = probe(&agent, question)?;

    let a = VoiceCase {
        profile: pa.id.clone(),
        title: pa.title.clone(),
        own: ma.matches(&call_a.text),
        other: mb.hits(&call_a.text),
        call: call_a,
    };
    let b = VoiceCase {
        profile: pb.id.clone(),
        title: pb.title.clone(),
        own: mb.matches(&call_b.text),
        other: ma.hits(&call_b.text),
        call: call_b,
    };
    let control_hits_a = ma.hits(&control.text);
    let control_hits_b = mb.hits(&control.text);

    let verdict = if a.own && b.own && !a.other && !b.other {
        ContextVerdict::Confirmed
    } else if !a.own && !b.own {
        // Оба ответа мимо своих подписей: профиль на голос не повлиял.
        ContextVerdict::Flat
    } else {
        ContextVerdict::Inconclusive
    };

    Ok(ProfileVoiceReport {
        asked_model,
        calls: 3,
        question: question.to_string(),
        a,
        b,
        control,
        control_hits_a,
        control_hits_b,
        verdict,
    })
}

/// «Учитывает автоматически»: записи `профиль.*` из долговременной памяти
/// подклеиваются в блок профиля, и без них ответ про пользователя
/// разваливается. Разница между двумя вызовами — ровно две строки блока.
pub fn run_profile_auto(model: Option<&str>) -> Res<ProfileAutoReport> {
    let settings = profile_settings(model)?;
    let asked_model = settings.model.clone();
    let token = "Евгений";
    let question = "Поздоровайся и скажи, сколько будет 12*12.";

    let mut agent = Agent::new(settings.clone())?;
    agent.set_profile("tutor")?;

    // Без памяти про пользователя: имя взять неоткуда.
    agent.set_memory(MemoryStore::in_memory());
    let without_memory = probe(&agent, question)?;

    let mut store = MemoryStore::in_memory();
    store.apply_ops(&[
        memory::Op::Upsert {
            key: "профиль.имя".into(),
            value: token.into(),
            layer: Some(Layer::Long),
        },
        memory::Op::Upsert {
            key: "профиль.обращение".into(),
            value: "здоровайся по имени в первой строке".into(),
            layer: Some(Layer::Long),
        },
    ]);
    agent.set_memory(store);
    let with_memory = probe(&agent, question)?;

    let with_recalled = has_word(&with_memory.text, token);
    let without_recalled = has_word(&without_memory.text, token);
    let bigger = with_memory.prompt_tokens > without_memory.prompt_tokens;

    let verdict = if with_recalled && !without_recalled && bigger {
        ContextVerdict::Confirmed
    } else if !with_recalled {
        ContextVerdict::Flat
    } else {
        ContextVerdict::Leaky
    };

    Ok(ProfileAutoReport {
        asked_model,
        calls: 2,
        profile: "tutor".into(),
        question: question.to_string(),
        token: token.to_string(),
        with_memory,
        without_memory,
        with_recalled,
        without_recalled,
        verdict,
    })
}

// ---------------------------------------------------------------------------
// Задача 13: состояние задачи как конечный автомат (`todo.rs`).
// ---------------------------------------------------------------------------

/// Что именно проверяем в тудушке.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TodoCheck {
    /// Сам автомат: законные переходы проходят, незаконные — отказ. Без сети.
    Machine,
    /// Что уезжает в `system`: блок один, в нём этап/шаг/ожидаемое действие
    /// и итоги пройденных этапов, а при переходе меняется **только** он.
    /// Без сети.
    Wire,
    /// Живьём: пауза и продолжение без повторных объяснений. Confirmed
    /// только когда то, что знает продолжение, не знает контроль без блока.
    Resume,
    /// Живьём: вся лестница на задаче с машинно-проверяемым ответом.
    Ladder,
}

impl TodoCheck {
    pub const ALL: [TodoCheck; 4] = [
        TodoCheck::Machine,
        TodoCheck::Wire,
        TodoCheck::Resume,
        TodoCheck::Ladder,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TodoCheck::Machine => "machine",
            TodoCheck::Wire => "wire",
            TodoCheck::Resume => "resume",
            TodoCheck::Ladder => "ladder",
        }
    }

    /// Не нужна ли этой проверке сеть.
    pub fn offline(self) -> bool {
        matches!(self, TodoCheck::Machine | TodoCheck::Wire)
    }

    /// `machine|wire|resume|ladder|all`.
    pub fn parse(s: &str) -> Res<Vec<TodoCheck>> {
        match s.trim().to_ascii_lowercase().as_str() {
            "all" | "" => Ok(TodoCheck::ALL.to_vec()),
            "offline" => Ok(vec![TodoCheck::Machine, TodoCheck::Wire]),
            "machine" | "fsm" | "states" => Ok(vec![TodoCheck::Machine]),
            "wire" | "prompt" | "system" => Ok(vec![TodoCheck::Wire]),
            "resume" | "pause" | "continue" => Ok(vec![TodoCheck::Resume]),
            "ladder" | "flow" | "stages" => Ok(vec![TodoCheck::Ladder]),
            other => Err(format!(
                "unknown todo check `{other}`; expected machine, wire, resume, ladder, offline or all"
            )),
        }
    }
}

/// Один пункт протокола автомата: что пробовали и что вышло.
#[derive(Clone, Debug)]
pub struct MachineStep {
    pub what: String,
    /// Чем кончилось: `ok` или текст отказа.
    pub got: String,
    /// Состояние после попытки.
    pub state: String,
    /// Совпало ли с ожиданием.
    pub ok: bool,
}

#[derive(Clone, Debug)]
pub struct TodoMachineReport {
    pub steps: Vec<MachineStep>,
    pub verdict: ContextVerdict,
}

/// Один случай сборки `system` с блоком состояния.
#[derive(Clone, Debug)]
pub struct TodoWireCase {
    pub label: String,
    pub stage: String,
    pub block: Option<String>,
    /// Сколько раз встретился заголовок блока: два блока — баг.
    pub blocks: usize,
    /// Всё, что не блок состояния.
    pub rest: String,
    /// Обязательные строки блока, которых в нём не нашлось.
    pub missing: Vec<String>,
}

impl TodoWireCase {
    fn line(&self) -> String {
        format!(
            "[{}] этап={} блоков={} потеряно строк={} остальное={} симв.",
            self.label,
            self.stage,
            self.blocks,
            self.missing.len(),
            self.rest.chars().count()
        )
    }
}

#[derive(Clone, Debug)]
pub struct TodoWireReport {
    pub cases: Vec<TodoWireCase>,
    /// Совпало ли «остальное» до и после перехода на следующий этап.
    pub rest_identical: bool,
    /// Отличаются ли сами блоки.
    pub blocks_differ: bool,
    /// Уехал ли итог закрытого этапа в блок.
    pub log_carried: bool,
    /// При `todo=off` блока нет вовсе.
    pub off_has_no_block: bool,
    pub verdict: ContextVerdict,
}

#[derive(Clone, Debug)]
pub struct TodoResumeReport {
    pub asked_model: String,
    pub calls: usize,
    /// Контрольный код, который есть только в итоге закрытого этапа.
    pub token: String,
    /// Что человек напечатал, продолжая работу.
    pub resume_prompt: String,
    /// Сколько пришлось бы напечатать, объясняя всё заново.
    pub cold_chars: usize,
    /// Продолжение: блок состояния на месте, человек сказал «продолжай».
    pub carried: Call,
    /// Контроль: тот же вопрос без блока состояния.
    pub control: Call,
    /// Контроль наоборот: без блока, но с полным пересказом в запросе.
    pub cold: Call,
    pub carried_has: bool,
    pub control_has: bool,
    pub cold_has: bool,
    pub verdict: ContextVerdict,
}

/// Один этап живого прогона лестницы.
#[derive(Clone, Debug)]
pub struct TodoStageCase {
    pub stage: String,
    /// Какой этап закрыл ответ (по маркеру); пусто — маркера не было.
    pub claimed: String,
    pub confirmed: bool,
    pub summary: String,
    pub call: Call,
}

impl TodoStageCase {
    fn line(&self) -> String {
        format!(
            "[{}] закрыл=`{}` подтверждён={} · {}\n  итог: {}",
            self.stage,
            if self.claimed.is_empty() {
                "—"
            } else {
                &self.claimed
            },
            self.confirmed,
            self.call.line(),
            clip(&self.summary, 160)
        )
    }
}

#[derive(Clone, Debug)]
pub struct TodoLadderReport {
    pub asked_model: String,
    pub calls: usize,
    pub task: String,
    /// Машинно-проверяемый ответ, посчитанный здесь же, а не вписанный руками.
    pub want: String,
    pub stages: Vec<TodoStageCase>,
    pub reached_done: bool,
    /// Есть ли правильное число в финальном ответе человеку.
    pub answer_ok: bool,
    pub verdict: ContextVerdict,
}

#[derive(Clone, Debug)]
pub enum TodoReport {
    Machine(TodoMachineReport),
    Wire(TodoWireReport),
    Resume(Box<TodoResumeReport>),
    Ladder(Box<TodoLadderReport>),
}

impl TodoReport {
    pub fn verdict(&self) -> ContextVerdict {
        match self {
            TodoReport::Machine(r) => r.verdict,
            TodoReport::Wire(r) => r.verdict,
            TodoReport::Resume(r) => r.verdict,
            TodoReport::Ladder(r) => r.verdict,
        }
    }

    pub fn confirmed(&self) -> bool {
        self.verdict() == ContextVerdict::Confirmed
    }

    pub fn calls(&self) -> usize {
        match self {
            TodoReport::Machine(_) | TodoReport::Wire(_) => 0,
            TodoReport::Resume(r) => r.calls,
            TodoReport::Ladder(r) => r.calls,
        }
    }

    pub fn status_line(&self) -> String {
        match self {
            TodoReport::Machine(r) => format!(
                "machine: verdict={} переходов={} провалов={}",
                r.verdict.as_str(),
                r.steps.len(),
                r.steps.iter().filter(|s| !s.ok).count()
            ),
            TodoReport::Wire(r) => format!(
                "wire: verdict={} случаев={} остальное не тронуто={} блоки различаются={} итог доехал={} off без блока={}",
                r.verdict.as_str(),
                r.cases.len(),
                r.rest_identical,
                r.blocks_differ,
                r.log_carried,
                r.off_has_no_block
            ),
            TodoReport::Resume(r) => format!(
                "resume: verdict={} код={} продолжение помнит={} контроль без блока={} пересказ помнит={} набрано человеком {}→{} симв. prompt_tokens {}→{}",
                r.verdict.as_str(),
                r.token,
                r.carried_has,
                r.control_has,
                r.cold_has,
                r.cold_chars,
                r.resume_prompt.chars().count(),
                r.control.prompt_tokens,
                r.carried.prompt_tokens
            ),
            TodoReport::Ladder(r) => format!(
                "ladder: verdict={} этапов подтверждено={}/{} дошли до done={} ответ={} (ждали {})",
                r.verdict.as_str(),
                r.stages.iter().filter(|s| s.confirmed).count(),
                r.stages.len(),
                r.reached_done,
                r.answer_ok,
                r.want
            ),
        }
    }

    pub fn render(&self) -> String {
        match self {
            TodoReport::Machine(r) => {
                let mut out = String::from(
                    "=== автомат состояния задачи (без сети) ===\n\
                     законные переходы проходят, незаконные — отказ\n\n",
                );
                for s in &r.steps {
                    out.push_str(&format!(
                        "{} {:<44} → {:<34} [{}]\n",
                        if s.ok { "\u{2713}" } else { "\u{2717}" },
                        s.what,
                        clip(&s.got, 34),
                        s.state
                    ));
                }
                out.push_str(&format!("\nverdict={}\n", r.verdict.as_str()));
                out
            }
            TodoReport::Wire(r) => {
                let mut out = String::from(
                    "=== состояние задачи на проводе (без сети) ===\n\
                     один и тот же запрос, меняется только этап\n\n",
                );
                for c in &r.cases {
                    out.push_str(&c.line());
                    out.push('\n');
                }
                out.push_str(&format!(
                    "\nодно состояние — один блок: {}\n\
                     в блоке есть этап, шаг и ожидаемое действие: {}\n\
                     итог закрытого этапа уехал в блок: {}\n\
                     при переходе остальное в system не изменилось: {}\n\
                     блоки двух этапов различаются: {}\n\
                     при todo=off блока нет вовсе: {}\n\
                     verdict={}\n",
                    r.cases.iter().all(|c| c.blocks <= 1),
                    r.cases.iter().all(|c| c.missing.is_empty()),
                    r.log_carried,
                    r.rest_identical,
                    r.blocks_differ,
                    r.off_has_no_block,
                    r.verdict.as_str()
                ));
                out
            }
            TodoReport::Resume(r) => {
                let mut out = format!(
                    "=== пауза и продолжение без повторных объяснений (живьём) ===\n\
                     модель: просили {}\n\
                     контрольный код живёт только в итоге закрытого этапа: {}\n\
                     человек печатает при продолжении: «{}» ({} симв.)\n\
                     пересказ того же своими словами стоил бы {} симв.\n\n",
                    r.asked_model,
                    r.token,
                    r.resume_prompt.trim(),
                    r.resume_prompt.chars().count(),
                    r.cold_chars
                );
                out.push_str(&format!(
                    "[продолжение, блок состояния на месте] {}\n  {}\n  код на месте: {}\n\n",
                    r.carried.line(),
                    clip(&r.carried.text, 200),
                    r.carried_has
                ));
                out.push_str(&format!(
                    "[контроль, блока нет] {}\n  {}\n  код на месте: {}\n\n",
                    r.control.line(),
                    clip(&r.control.text, 200),
                    r.control_has
                ));
                out.push_str(&format!(
                    "[контроль наоборот: блока нет, но всё пересказано] {}\n  {}\n  код на месте: {}\n",
                    r.cold.line(),
                    clip(&r.cold.text, 200),
                    r.cold_has
                ));
                out.push_str(&format!(
                    "\nпродолжение знает то, чего не знает контроль → verdict={}\n",
                    r.verdict.as_str()
                ));
                out
            }
            TodoReport::Ladder(r) => {
                let mut out = format!(
                    "=== лестница этапов целиком (живьём) ===\n\
                     модель: просили {}\n\
                     задача: {}\n\
                     машинно-проверяемый ответ: {}\n\n",
                    r.asked_model, r.task, r.want
                );
                for s in &r.stages {
                    out.push_str(&s.line());
                    out.push_str("\n\n");
                }
                out.push_str(&format!(
                    "каждый этап закрыл сам себя: {}\n\
                     автомат дошёл до done: {}\n\
                     финальный ответ содержит {}: {}\n\
                     verdict={}\n",
                    r.stages.iter().all(|s| s.confirmed),
                    r.reached_done,
                    r.want,
                    r.answer_ok,
                    r.verdict.as_str()
                ));
                out
            }
        }
    }
}

pub fn run_todo(checks: &[TodoCheck], model: Option<&str>) -> Res<Vec<TodoReport>> {
    let mut out = Vec::new();
    for check in checks {
        out.push(match check {
            TodoCheck::Machine => TodoReport::Machine(run_todo_machine()),
            TodoCheck::Wire => TodoReport::Wire(run_todo_wire()),
            TodoCheck::Resume => TodoReport::Resume(Box::new(run_todo_resume(model)?)),
            TodoCheck::Ladder => TodoReport::Ladder(Box::new(run_todo_ladder(model)?)),
        });
    }
    Ok(out)
}

/// Один пункт протокола: что пробовали, чего ждали и что вышло.
fn machine_step(what: &str, got: Res<String>, want_ok: bool, st: &TaskState) -> MachineStep {
    MachineStep {
        what: what.to_string(),
        got: match &got {
            Ok(s) if s.is_empty() => "ok".to_string(),
            Ok(s) => s.clone(),
            Err(e) => format!("отказ: {e}"),
        },
        state: state_line(st),
        ok: got.is_ok() == want_ok,
    }
}

/// Короткая запись состояния для протокола.
fn state_line(st: &TaskState) -> String {
    format!("{}/{}/шаг {}", st.status.as_str(), st.stage.id(), st.step)
}

/// Протокол автомата: каждая строка — попытка перехода и то, чем она
/// кончилась. Проверка не в том, что «ничего не упало», а в том, что
/// незаконный переход именно **отказ**, и состояние после него не поехало.
pub fn run_todo_machine() -> TodoMachineReport {
    let mut steps = Vec::new();
    let mut st = TaskState::new();


    // Пустое состояние: двигать нечего.
    let r = st.advance("нечего").map(|s| s.id().to_string());
    steps.push(machine_step("advance из idle", r, false, &st));
    let r = st.pause("нечего").map(|_| String::new());
    steps.push(machine_step("pause из idle", r, false, &st));
    let r = st.resume().map(|_| String::new());
    steps.push(machine_step("resume из idle", r, false, &st));

    let r = st.start("посчитать буквы").map(|_| String::new());
    steps.push(machine_step("start из idle", r, true, &st));
    let r = st.start("другая задача").map(|_| String::new());
    steps.push(machine_step("start поверх идущей задачи", r, false, &st));

    // Лестница целиком.
    for want in [
        todo::Stage::Plan,
        todo::Stage::Execute,
        todo::Stage::Validate,
    ] {
        let got = st.advance("итог этапа").map(|s| s.id().to_string());
        let matched = got.as_deref() == Ok(want.id());
        let r = if matched {
            got
        } else {
            Err(format!("ожидали {}, получили {got:?}", want.id()))
        };
        steps.push(machine_step(&format!("advance → {}", want.id()), r, true, &st));
    }

    // Пауза держит этап и шаг, и двигаться с неё нельзя.
    let before = state_line(&st);
    let r = st.pause("человек ушёл").map(|_| String::new());
    steps.push(machine_step("pause на validate", r, true, &st));
    let r = st.advance("тайком").map(|s| s.id().to_string());
    steps.push(machine_step("advance на паузе", r, false, &st));
    let held = state_line(&st) == format!("paused/{}", before.trim_start_matches("running/"));
    steps.push(MachineStep {
        what: "пауза сохранила этап и шаг".into(),
        got: if held {
            "ok".into()
        } else {
            format!("было {before}, стало {}", state_line(&st))
        },
        state: state_line(&st),
        ok: held,
    });

    // Продолжение — с того же места, а не с начала.
    let r = st.resume().map(|_| String::new());
    steps.push(machine_step("resume с паузы", r, true, &st));
    let same = st.stage == todo::Stage::Validate && st.step == 4;
    steps.push(MachineStep {
        what: "продолжили тем же этапом и шагом".into(),
        got: if same { "ok".into() } else { state_line(&st) },
        state: state_line(&st),
        ok: same,
    });

    let r = st.advance("проверено").map(|s| s.id().to_string());
    steps.push(machine_step("advance → report", r, true, &st));
    let r = st.advance("отписался").map(|s| s.id().to_string());
    steps.push(machine_step("advance → done", r, true, &st));

    // Done терминален.
    let r = st.advance("ещё разок").map(|s| s.id().to_string());
    steps.push(machine_step("advance из done", r, false, &st));
    let r = st.pause("ещё разок").map(|_| String::new());
    steps.push(machine_step("pause из done", r, false, &st));
    let r = st.resume().map(|_| String::new());
    steps.push(machine_step("resume из done", r, false, &st));

    // Состояние переживает файл сессии — иначе пауза не пережила бы выход.
    let raw = serde_json::to_string(&st).unwrap_or_default();
    let back: Res<TaskState> = serde_json::from_str(&raw).map_err(|e| e.to_string());
    let survived = back.as_ref().map(|b| b == &st).unwrap_or(false);
    steps.push(MachineStep {
        what: "состояние пережило запись и чтение JSON".into(),
        got: if survived {
            "ok".into()
        } else {
            "состояние не совпало".into()
        },
        state: state_line(&st),
        ok: survived,
    });

    let verdict = if steps.iter().all(|s| s.ok) {
        ContextVerdict::Confirmed
    } else {
        ContextVerdict::Inconclusive
    };
    TodoMachineReport { steps, verdict }
}

/// Агент с готовым состоянием задачи и без файлов контекста: проверяем
/// сборку `system`, а не окружение машины.
fn todo_agent(state: TaskState, todo_on: bool) -> Agent {
    let settings = Settings {
        system_prompt: CUSTOM_PROMPT.to_string(),
        context_enabled: false,
        todo: todo_on,
        ..Settings::default()
    };
    let mut agent = Agent::with_endpoint(api::Endpoint::unusable(), settings);
    agent.set_todo(state);
    agent
}

fn todo_wire_case(label: &str, agent: &Agent) -> TodoWireCase {
    let system = agent.system_for_request();
    let (block, rest) = todo::split_block(&system);
    let st = agent.todo();
    let body = block.clone().unwrap_or_default();
    // Три обязательные строки — ровно то, чего требовала постановка.
    let want = [
        format!("Этап задачи: {}", st.stage.index()),
        format!("Текущий шаг: {}", st.step),
        format!("Ожидаемое действие: {}", st.stage.expected()),
    ];
    let missing = if agent.todo_on_wire() {
        want.into_iter().filter(|w| !body.contains(w)).collect()
    } else {
        Vec::new()
    };
    TodoWireCase {
        label: label.to_string(),
        stage: st.stage.id().to_string(),
        blocks: system.matches(todo::BLOCK_HEAD).count(),
        block,
        rest,
        missing,
    }
}

/// Что реально уезжает в `system`. Ноль запросов: сборку видно целиком.
pub fn run_todo_wire() -> TodoWireReport {
    let mut cases = Vec::new();

    // 1. Тудушка выключена — блока нет, сколько бы состояния ни накопилось.
    let mut walked = TaskState::new();
    let _ = walked.start("посчитать буквы «о» в строке");
    let _ = walked.advance("критерий готовности: число совпадает с пересчётом");
    let off = todo_agent(walked.clone(), false);
    cases.push(todo_wire_case("todo=off", &off));
    let off_has_no_block = cases[0].block.is_none() && cases[0].blocks == 0;

    // 2. Тудушка включена, этап `plan`.
    let on_plan = todo_agent(walked.clone(), true);
    cases.push(todo_wire_case("этап plan", &on_plan));
    let log_carried = cases[1]
        .block
        .as_deref()
        .is_some_and(|b| b.contains("число совпадает с пересчётом"));

    // 3. Переход на следующий этап: поменяться должен только блок.
    let mut next = walked.clone();
    let _ = next.advance("план из трёх шагов, проверка пересчётом");
    let on_exec = todo_agent(next.clone(), true);
    cases.push(todo_wire_case("этап execute", &on_exec));
    let rest_identical = cases[1].rest == cases[2].rest && cases[1].rest.contains(CUSTOM_PROMPT);
    let blocks_differ = cases[1].block != cases[2].block;

    // 4. Пауза видна в блоке — иначе модель не знает, что стоять.
    let mut paused = next.clone();
    let _ = paused.pause("человек ушёл за чаем");
    let on_pause = todo_agent(paused, true);
    cases.push(todo_wire_case("пауза", &on_pause));
    let pause_visible = cases[3]
        .block
        .as_deref()
        .is_some_and(|b| b.contains("status=\"paused\"") && b.contains("причина паузы"));

    let verdict = if cases.iter().any(|c| c.blocks > 1 || !c.missing.is_empty())
        || !off_has_no_block
        || !log_carried
        || !rest_identical
        || !blocks_differ
        || !pause_visible
    {
        ContextVerdict::Inconclusive
    } else {
        ContextVerdict::Confirmed
    };

    TodoWireReport {
        cases,
        rest_identical,
        blocks_differ,
        log_carried,
        off_has_no_block,
        verdict,
    }
}

/// Настройки живых проверок тудушки: та же изоляция, что у профиля, плюс
/// включённая настройка `todo` и `temperature=0`.
fn todo_settings(model: Option<&str>) -> Res<Settings> {
    let mut settings = context_settings(model)?;
    settings.temperature = Some(0.0);
    settings.todo = true;
    Ok(settings)
}

/// Задача с машинно-проверяемым ответом: число считается здесь же, а не
/// вписывается руками, поэтому проверять ответ можно без доверия к автору.
const TODO_PHRASE: &str = "колокольчик подорожник одуванчик";

fn todo_task() -> (String, String) {
    let want = TODO_PHRASE.chars().filter(|c| *c == 'о').count();
    (
        format!("Посчитай, сколько раз буква «о» встречается в строке «{TODO_PHRASE}»."),
        want.to_string(),
    )
}

/// Пауза и продолжение. Контрольный код лежит **только** в итоге закрытого
/// этапа — не в формулировке задачи и не в истории: история во всех трёх
/// вызовах пуста. Значит, единственный путь, которым он может доехать до
/// модели, — блок состояния. Отсюда и контроль: тот же запрос без блока
/// знать код неоткуда.
pub fn run_todo_resume(model: Option<&str>) -> Res<TodoResumeReport> {
    let settings = todo_settings(model)?;
    let asked_model = settings.model.clone();
    let token = "ЖЕЛУДЬ-41";
    let (task, _) = todo_task();
    let summary = format!(
        "критерий готовности: число совпадает с независимым пересчётом; контрольный код задачи {token}"
    );

    // Состояние: этап `study` закрыт, стоим на `plan`, пауза.
    let mut state = TaskState::new();
    state.start(&task)?;
    state.advance(&summary)?;
    state.pause("человек ушёл за чаем")?;

    let resume_prompt = "Продолжай.\nВ первой строке повтори контрольный код задачи из итога предыдущего этапа (если его нет — напиши НЕТ).";
    // То же самое, но объяснённое заново — во сколько обошлось бы человеку.
    let cold_prompt = format!(
        "Мы решаем задачу: {task}\nЭтап «изучить» уже закрыт, его итог: {summary}\nСейчас этап «запланировать», шаг 2.\n{resume_prompt}"
    );
    let cold_chars = cold_prompt.chars().count();

    let mut agent = Agent::new(settings.clone())?;
    let mut carried_state = state.clone();
    carried_state.resume()?;
    agent.set_todo(carried_state);
    let carried = probe(&agent, resume_prompt)?;

    // Контроль: тот же запрос, блока состояния нет.
    agent.settings_mut().todo = false;
    let control = probe(&agent, resume_prompt)?;
    // Контроль наоборот: блока нет, но всё пересказано в запросе. Нужен,
    // чтобы отличить «блок не доехал» от «модель просто не умеет».
    let cold = probe(&agent, &cold_prompt)?;

    let has = |c: &Call| c.text.to_uppercase().contains(token);
    let carried_has = has(&carried);
    let control_has = has(&control);
    let cold_has = has(&cold);

    let verdict = match (carried_has, control_has, cold_has) {
        (true, false, true) => ContextVerdict::Confirmed,
        // Код всплыл и без блока — эксперимент не про блок.
        (_, true, _) => ContextVerdict::Leaky,
        // Ни один вызов кода не назвал: проверять нечего.
        (false, false, false) => ContextVerdict::Flat,
        _ => ContextVerdict::Inconclusive,
    };

    Ok(TodoResumeReport {
        asked_model,
        calls: 3,
        token: token.to_string(),
        resume_prompt: resume_prompt.to_string(),
        cold_chars,
        carried,
        control,
        cold,
        carried_has,
        control_has,
        cold_has,
        verdict,
    })
}

/// Вся лестница живьём, ровно так же, как её гоняет TUI: на каждом этапе
/// отдельный запрос, автомат двигается только на подтверждении этапа.
pub fn run_todo_ladder(model: Option<&str>) -> Res<TodoLadderReport> {
    let settings = todo_settings(model)?;
    let asked_model = settings.model.clone();
    let (task, want) = todo_task();

    let mut agent = Agent::new(settings)?;
    let mut state = TaskState::new();
    state.start(&task)?;
    agent.set_todo(state.clone());

    let mut history: Vec<ChatMessage> = Vec::new();
    let mut stages = Vec::new();
    let mut report_text = String::new();

    while state.running() {
        agent.set_todo(state.clone());
        let prompt = state.stage_prompt();
        let call = ask_call(&agent, &history, &prompt)?;
        history.push(ChatMessage::user(prompt));
        history.push(ChatMessage::assistant(call.text.clone()));

        let claimed = todo::parse_completion(&call.text)
            .map(|(s, _)| s.id().to_string())
            .unwrap_or_default();
        let stage_id = state.stage.id().to_string();
        let confirmed = state.confirm(&call.text);
        let summary = match &confirmed {
            Ok(s) => s.clone(),
            Err(e) => e.clone(),
        };
        if state.stage == todo::Stage::Report {
            report_text = call.text.clone();
        }
        stages.push(TodoStageCase {
            stage: stage_id,
            claimed,
            confirmed: confirmed.is_ok(),
            summary: summary.clone(),
            call,
        });
        match confirmed {
            Ok(s) => {
                state.advance(&s)?;
            }
            // Этап себя не закрыл — автомат встаёт, а не едет дальше.
            Err(e) => {
                state.pause(&e)?;
                break;
            }
        }
    }

    let reached_done = state.finished();
    let answer_ok = report_text.contains(&want);
    let all_confirmed = stages.iter().all(|s| s.confirmed) && stages.len() == todo::Stage::LADDER.len();
    let verdict = if all_confirmed && reached_done && answer_ok {
        ContextVerdict::Confirmed
    } else if stages.iter().all(|s| !s.confirmed) {
        ContextVerdict::Flat
    } else {
        ContextVerdict::Inconclusive
    };

    Ok(TodoLadderReport {
        asked_model,
        calls: stages.len(),
        task,
        want,
        stages,
        reached_done,
        answer_ok,
        verdict,
    })
}

// ---------------------------------------------------------------------------
// Задача 15: контролируемый жизненный цикл задачи (`run.rs`).
// ---------------------------------------------------------------------------

/// Что именно доказываем про жизненный цикл.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LifeCheck {
    /// Вся таблица переходов. Каждый незаконный переход обязан быть отказом
    /// **и** оставить прогон побайтно тем же. Без сети.
    Machine,
    /// Живьём: модели прямым текстом велят пропустить план и сразу выдать
    /// финальный ответ. Confirmed — только если прогон остался на `plan`.
    Gate,
    /// Прогон поднимается **с диска** (не из памяти) и продолжается с того
    /// же этапа. Офлайн-половина — файл сессии; живая — что продолжению не
    /// нужно переобъяснять задачу.
    Resume,
    /// Где прогон лежит и когда исчезает: во время работы есть, после
    /// паузы и перезагрузки — тот же, после `done` — нет, вместе с
    /// сессией — нет. Без сети.
    Cleanup,
}

impl LifeCheck {
    pub const ALL: [LifeCheck; 4] = [
        LifeCheck::Machine,
        LifeCheck::Gate,
        LifeCheck::Resume,
        LifeCheck::Cleanup,
    ];

    pub fn label(self) -> &'static str {
        match self {
            LifeCheck::Machine => "machine",
            LifeCheck::Gate => "gate",
            LifeCheck::Resume => "resume",
            LifeCheck::Cleanup => "cleanup",
        }
    }

    pub fn offline(self) -> bool {
        matches!(self, LifeCheck::Machine | LifeCheck::Cleanup)
    }

    pub fn parse(s: &str) -> Res<Vec<LifeCheck>> {
        match s.trim().to_ascii_lowercase().as_str() {
            "all" | "" => Ok(LifeCheck::ALL.to_vec()),
            "offline" => Ok(vec![LifeCheck::Machine, LifeCheck::Cleanup]),
            "machine" | "table" | "states" => Ok(vec![LifeCheck::Machine]),
            "gate" | "approve" | "plan" => Ok(vec![LifeCheck::Gate]),
            "resume" | "continue" | "pause" => Ok(vec![LifeCheck::Resume]),
            "cleanup" | "storage" | "disk" => Ok(vec![LifeCheck::Cleanup]),
            other => Err(format!(
                "unknown lifecycle check `{other}`; expected machine, gate, resume, cleanup, offline or all"
            )),
        }
    }
}

/// Один пункт протокола жизненного цикла.
#[derive(Clone, Debug)]
pub struct LifeStep {
    pub what: String,
    pub got: String,
    pub state: String,
    pub ok: bool,
}

#[derive(Clone, Debug)]
pub struct LifeReport {
    pub label: &'static str,
    pub asked_model: String,
    pub calls: usize,
    pub steps: Vec<LifeStep>,
    pub notes: Vec<String>,
    pub verdict: ContextVerdict,
}

impl LifeReport {
    pub fn confirmed(&self) -> bool {
        self.verdict == ContextVerdict::Confirmed
    }

    pub fn status_line(&self) -> String {
        format!(
            "{}: verdict={} пунктов={} провалов={}",
            self.label,
            self.verdict.as_str(),
            self.steps.len(),
            self.steps.iter().filter(|s| !s.ok).count()
        )
    }

    pub fn render(&self) -> String {
        let mut out = format!("== жизненный цикл задачи: {} ==\n", self.label);
        if !self.asked_model.is_empty() {
            out.push_str(&format!("модель: {} · живых вызовов: {}\n", self.asked_model, self.calls));
        }
        for s in &self.steps {
            out.push_str(&format!(
                "  [{}] {} -> {} | состояние: {}\n",
                if s.ok { "ok" } else { "ПРОВАЛ" },
                s.what,
                s.got,
                s.state
            ));
        }
        for n in &self.notes {
            out.push_str(&format!("  \u{b7} {n}\n"));
        }
        out.push_str(&format!("verdict: {}\n", self.verdict.as_str()));
        out
    }
}

pub fn run_lifecycle(checks: &[LifeCheck], model: Option<&str>) -> Res<Vec<LifeReport>> {
    let mut out = Vec::new();
    for check in checks {
        out.push(match check {
            LifeCheck::Machine => life_machine(),
            LifeCheck::Cleanup => life_cleanup()?,
            LifeCheck::Gate => life_gate(model)?,
            LifeCheck::Resume => life_resume(model)?,
        });
    }
    Ok(out)
}

/// Слепок прогона без журнала и часов: журнал растёт и на отказах (он их и
/// фиксирует), `updated_at` тикает. Сравнивать «состояние не поехало» нужно
/// по остальному, иначе проверка доказывала бы обратное.
fn run_fingerprint(run: &crate::run::TaskRun) -> String {
    let mut v: serde_json::Value = serde_json::to_value(run).unwrap_or(serde_json::Value::Null);
    if let Some(obj) = v.as_object_mut() {
        obj.remove("log");
        obj.remove("updated_at");
    }
    v.to_string()
}

fn life_step(
    what: &str,
    got: Result<crate::run::Position, crate::run::Refusal>,
    want_ok: bool,
    run: &crate::run::TaskRun,
) -> LifeStep {
    LifeStep {
        what: what.to_string(),
        got: match &got {
            Ok(p) => p.line(),
            Err(r) => format!("отказ: {}", r.why),
        },
        state: run.position().line(),
        ok: got.is_ok() == want_ok,
    }
}

/// Протокол по всей таблице переходов. Два обязательства на каждый
/// незаконный переход: он **отказ**, и прогон после него побайтно тот же.
pub fn life_machine() -> LifeReport {
    use crate::run::{ApprovePolicy, Event, Phase, RunStatus, TaskRun};
    use crate::todo::{Stage, Verdict};

    let mut steps = Vec::new();
    let mut notes = Vec::new();
    let settings = Settings::default();
    let mut run = TaskRun::new("verify-lifecycle", &settings);

    // Каждый незаконный переход проверяется парой: отказ + неподвижность.
    let illegal = |run: &mut TaskRun, what: &str, ev: Event, steps: &mut Vec<LifeStep>| {
        let before = run_fingerprint(run);
        let got = run.apply(ev);
        let refused = got.is_err();
        steps.push(life_step(what, got, false, run));
        let held = run_fingerprint(run) == before;
        steps.push(LifeStep {
            what: format!("{what}: состояние не сдвинулось"),
            got: if held { "побайтно то же".into() } else { "СОСТОЯНИЕ ПОЕХАЛО".into() },
            state: run.position().line(),
            ok: held && refused,
        });
    };

    illegal(&mut run, "start с пустой формулировкой", Event::Start("   ".into()), &mut steps);
    illegal(&mut run, "закрыть этап до старта", Event::StageClosed { summary: "а".into(), verdict: None }, &mut steps);
    illegal(&mut run, "resume до старта", Event::Resume, &mut steps);

    let got = run.apply(Event::Start("посчитать буквы".into()));
    steps.push(life_step("start", got, true, &run));
    illegal(&mut run, "start поверх идущего прогона", Event::Start("другая".into()), &mut steps);

    let got = run.apply(Event::StageClosed { summary: "изучил".into(), verdict: None });
    steps.push(life_step("study закрыт -> plan", got, true, &run));

    illegal(&mut run, "approve, когда никто не ждёт подписи", Event::Approve("я".into()), &mut steps);
    illegal(&mut run, "закрыть этап без итога", Event::StageClosed { summary: "  ".into(), verdict: None }, &mut steps);

    let got = run.apply(Event::StageClosed { summary: "план из трёх шагов".into(), verdict: None });
    let gated = run.status == RunStatus::AwaitingApproval && run.state.stage == Stage::Plan;
    steps.push(life_step("plan закрыт", got, true, &run));
    steps.push(LifeStep {
        what: "plan ушёл под подпись, а не в execute".into(),
        got: if gated { "awaiting-approval/plan".into() } else { run.position().line() },
        state: run.position().line(),
        ok: gated,
    });

    // Главный пункт постановки: реализация до утверждённого плана.
    illegal(&mut run, "execute до утверждения плана (stage-closed)", Event::StageClosed { summary: "сделал".into(), verdict: None }, &mut steps);
    illegal(&mut run, "execute до утверждения плана (смена фазы)", Event::PhaseTo(Phase::Model), &mut steps);
    illegal(&mut run, "resume из-под гейта", Event::Resume, &mut steps);
    illegal(&mut run, "pause из-под гейта", Event::Pause("устал".into()), &mut steps);
    illegal(&mut run, "reject без причины", Event::Reject("  ".into()), &mut steps);

    let got = run.apply(Event::Reject("шагов слишком много".into()));
    let back = run.state.stage == Stage::Plan && run.running();
    steps.push(life_step("reject -> переделка плана", got, true, &run));
    steps.push(LifeStep {
        what: "reject вернул на plan, а не двинул вперёд".into(),
        got: run.position().line(),
        state: run.position().line(),
        ok: back,
    });

    let got = run.apply(Event::StageClosed { summary: "план из двух шагов".into(), verdict: None });
    steps.push(life_step("plan закрыт повторно", got, true, &run));
    let got = run.apply(Event::Approve("человек".into()));
    let opened = run.state.stage == Stage::Execute && run.running();
    steps.push(life_step("approve -> execute", got, true, &run));
    steps.push(LifeStep {
        what: "подпись записана в прогон".into(),
        got: run.approvals.join("; "),
        state: run.position().line(),
        ok: opened && run.approvals.iter().any(|a| a.starts_with("plan:")),
    });

    let got = run.apply(Event::StageClosed { summary: "сделал".into(), verdict: None });
    steps.push(life_step("execute закрыт -> validate", got, true, &run));

    // Второй гейт: финала без вердикта не бывает.
    illegal(&mut run, "report без машинного вердикта", Event::StageClosed { summary: "вроде норм".into(), verdict: None }, &mut steps);

    let got = run.apply(Event::StageClosed { summary: "нашёл дефект".into(), verdict: Some(Verdict::NotOk) });
    let sent_back = run.state.stage == Stage::Execute;
    steps.push(life_step("validate(не ok) -> переделка", got, true, &run));
    steps.push(LifeStep {
        what: "«не ok» вернул в execute, а не в report".into(),
        got: run.position().line(),
        state: run.position().line(),
        ok: sent_back,
    });

    let got = run.apply(Event::StageClosed { summary: "починил".into(), verdict: None });
    steps.push(life_step("execute закрыт -> validate", got, true, &run));
    let got = run.apply(Event::StageClosed { summary: "чисто".into(), verdict: Some(Verdict::Ok) });
    let to_report = run.state.stage == Stage::Report;
    steps.push(life_step("validate(ok) -> report", got, true, &run));
    steps.push(LifeStep {
        what: "report достижим только с вердиктом ok".into(),
        got: run.position().line(),
        state: run.position().line(),
        ok: to_report,
    });

    // Пауза держит этап и шаг.
    let (stage, step) = (run.state.stage, run.state.step);
    let got = run.apply(Event::Pause("Esc".into()));
    steps.push(life_step("pause на report", got, true, &run));
    illegal(&mut run, "advance на паузе", Event::StageClosed { summary: "тайком".into(), verdict: None }, &mut steps);
    illegal(&mut run, "approve на паузе", Event::Approve("я".into()), &mut steps);
    illegal(&mut run, "смена фазы на паузе", Event::PhaseTo(Phase::Model), &mut steps);
    let got = run.apply(Event::Resume);
    let same = run.state.stage == stage && run.state.step == step;
    steps.push(life_step("resume", got, true, &run));
    steps.push(LifeStep {
        what: "продолжили тем же этапом и шагом".into(),
        got: format!("{}/шаг {}", run.state.stage.id(), run.state.step),
        state: run.position().line(),
        ok: same,
    });
    illegal(&mut run, "resume на идущем прогоне", Event::Resume, &mut steps);

    let got = run.apply(Event::StageClosed { summary: "отписался".into(), verdict: None });
    let done = run.status == RunStatus::DonePass;
    steps.push(life_step("report закрыт -> done(pass)", got, true, &run));
    steps.push(LifeStep {
        what: "done(pass) достижим только из report".into(),
        got: run.status.as_str().to_string(),
        state: run.position().line(),
        ok: done,
    });

    // Терминал: не ведёт ничего.
    for (what, ev) in [
        ("resume из done", Event::Resume),
        ("pause из done", Event::Pause("x".into())),
        ("approve из done", Event::Approve("x".into())),
        ("смена фазы из done", Event::PhaseTo(Phase::Model)),
        ("закрыть этап из done", Event::StageClosed { summary: "ещё".into(), verdict: None }),
        ("start из done", Event::Start("другая".into())),
        ("abort из done", Event::Abort("x".into())),
    ] {
        illegal(&mut run, what, ev, &mut steps);
    }

    // `auto` — подпись, а не отключённый гейт.
    let auto_settings = Settings { approve: ApprovePolicy::Auto, ..Settings::default() };
    let mut auto = TaskRun::new("verify-auto", &auto_settings);
    auto.apply(Event::Start("задача".into())).ok();
    auto.apply(Event::StageClosed { summary: "изучил".into(), verdict: None }).ok();
    auto.apply(Event::StageClosed { summary: "план".into(), verdict: None }).ok();
    let via_gate = auto.status == RunStatus::AwaitingApproval;
    steps.push(LifeStep {
        what: "approve=auto всё равно входит в гейт".into(),
        got: auto.position().line(),
        state: auto.position().line(),
        ok: via_gate,
    });
    auto.apply(crate::run::Event::Approve("auto".into())).ok();
    let logged = auto.log.iter().any(|e| e.event == "approve" && e.detail == "auto" && e.ok);
    steps.push(LifeStep {
        what: "подпись `auto` лежит в журнале переходов".into(),
        got: if logged { "approved-by=auto".into() } else { "записи нет".into() },
        state: auto.position().line(),
        ok: logged && auto.state.stage == Stage::Execute,
    });

    // Журнал: отказы в нём есть, и это и есть «реакция на попытку прыжка».
    let refusals = run.log.iter().filter(|e| !e.ok).count();
    steps.push(LifeStep {
        what: "каждая попытка прыжка записана в журнал".into(),
        got: format!("отказов в журнале: {refusals}"),
        state: run.position().line(),
        ok: refusals >= 20,
    });
    notes.push(format!(
        "последний отказ дословно: {}",
        run.log.iter().rev().find(|e| !e.ok).map(|e| e.line()).unwrap_or_default()
    ));

    let verdict = if steps.iter().all(|s| s.ok) {
        ContextVerdict::Confirmed
    } else {
        ContextVerdict::Inconclusive
    };
    LifeReport {
        label: "machine",
        asked_model: String::new(),
        calls: 0,
        steps,
        notes,
        verdict,
    }
}

/// Где прогон лежит и когда исчезает. Читаем **файл сессии с диска** в
/// четырёх точках: во время работы, после паузы и перезагрузки, после
/// `done(pass)` и после `done(fail)`. Плюс проверяем, что удаление сессии
/// уносит прогон с собой — это второе требование постановки про хранение.
pub fn life_cleanup() -> Res<LifeReport> {
    use crate::run::{Event, Phase, RunStatus, TaskRun};
    use crate::session::{self, Session};

    let dir = std::env::temp_dir().join(format!(
        "ask-lifecycle-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let settings = Settings::default();
    let mut session = Session::new(settings.clone());
    session.push_user("посчитать буквы".into());
    let id = session.id.clone();

    let mut steps = Vec::new();
    let mut notes = vec![format!("файлы сессий: {}", dir.display())];

    let mut run = TaskRun::new(&id, &settings);
    run.apply(Event::Start("посчитать буквы".into())).map_err(|r| r.why)?;
    run.apply(Event::StageClosed { summary: "изучил".into(), verdict: None }).map_err(|r| r.why)?;
    // Фаза на диск ДО «сетевого вызова» — ровно то, что делает драйвер.
    run.apply(Event::PhaseTo(Phase::Model)).map_err(|r| r.why)?;
    run.set_attempt(2);
    session.set_run(Some(run.clone()));
    session.save(&dir)?;

    // (1) Во время прогона он в файле, статус не терминальный.
    let raw = std::fs::read_to_string(dir.join(format!("{id}.json"))).map_err(|e| e.to_string())?;
    let on_disk = raw.contains("\"run\"") && raw.contains("\"model\"");
    let mid = session::load_session(&dir, &id)?;
    let mid_run = mid.run().cloned();
    let mid_ok = mid_run.as_ref().is_some_and(|r| {
        r.state.stage == crate::todo::Stage::Plan && r.attempt == 2 && !r.status.terminal()
    });
    steps.push(LifeStep {
        what: "(1) во время прогона он лежит в файле сессии".into(),
        got: mid_run
            .as_ref()
            .map(|r| format!("{} · попытка {}", r.position().line(), r.attempt))
            .unwrap_or_else(|| "run == null".into()),
        state: format!("файл {} байт", raw.len()),
        ok: on_disk && mid_ok,
    });
    // Фаза, записанная до сетевого вызова, — это и есть доказательство, что
    // падение посреди запроса видно на диске: загрузка поднимает прогон
    // прерванным, а не «ничего не было».
    let interrupted = mid_run.as_ref().is_some_and(|r| r.status == RunStatus::Interrupted);
    steps.push(LifeStep {
        what: "(1а) идущий прогон поднят с диска как interrupted".into(),
        got: mid_run.as_ref().map(|r| r.status.as_str().to_string()).unwrap_or_default(),
        state: "после Session::load".into(),
        ok: interrupted,
    });

    // (2) Пауза + перезагрузка: тот же этап и шаг.
    let mut paused = mid_run.clone().ok_or("прогон пропал")?;
    paused.apply(Event::Resume).map_err(|r| r.why)?;
    let (stage, step) = (paused.state.stage, paused.state.step);
    paused.apply(Event::Pause("Esc".into())).map_err(|r| r.why)?;
    let mut s2 = session.clone();
    s2.set_run(Some(paused.clone()));
    s2.save(&dir)?;
    let back = session::load_session(&dir, &id)?;
    let same = back
        .run()
        .is_some_and(|r| r.state.stage == stage && r.state.step == step && r.status == RunStatus::Paused);
    steps.push(LifeStep {
        what: "(2) после паузы и перезагрузки — тот же этап и шаг".into(),
        got: back.run().map(|r| r.position().line()).unwrap_or_else(|| "run == null".into()),
        state: format!("ждали {}/шаг {}", stage.id(), step),
        ok: same,
    });

    // (3) done(pass) — прогон снят с сессии.
    let mut finished = back.run().cloned().ok_or("прогон пропал")?;
    finished.apply(Event::Resume).map_err(|r| r.why)?;
    for (summary, verdict) in [
        ("план".to_string(), None),
        (String::new(), None), // approve вместо закрытия — см. ниже
    ] {
        if summary.is_empty() {
            finished.apply(Event::Approve("auto".into())).map_err(|r| r.why)?;
        } else {
            finished.apply(Event::StageClosed { summary, verdict }).map_err(|r| r.why)?;
        }
    }
    finished.apply(Event::StageClosed { summary: "сделал".into(), verdict: None }).map_err(|r| r.why)?;
    finished
        .apply(Event::StageClosed { summary: "чисто".into(), verdict: Some(crate::todo::Verdict::Ok) })
        .map_err(|r| r.why)?;
    finished.apply(Event::StageClosed { summary: "отписался".into(), verdict: None }).map_err(|r| r.why)?;
    let mut s3 = session.clone();
    s3.set_run(Some(finished.clone()));
    s3.save(&dir)?;
    let after_pass = session::load_session(&dir, &id)?;
    steps.push(LifeStep {
        what: "(3) после done(pass) прогон снят с сессии".into(),
        got: match after_pass.run() {
            Some(r) => format!("ОСТАЛСЯ: {}", r.position().line()),
            None => "run == null".into(),
        },
        state: finished.status.as_str().into(),
        ok: after_pass.run().is_none() && finished.status == RunStatus::DonePass,
    });

    // (4) done(fail) — так же.
    let mut failed = TaskRun::new(&id, &settings);
    failed.apply(Event::Start("бросим".into())).map_err(|r| r.why)?;
    failed.apply(Event::Abort("передумали".into())).map_err(|r| r.why)?;
    let mut s4 = session.clone();
    s4.set_run(Some(failed.clone()));
    s4.save(&dir)?;
    let after_fail = session::load_session(&dir, &id)?;
    steps.push(LifeStep {
        what: "(4) после done(fail) прогон снят с сессии".into(),
        got: match after_fail.run() {
            Some(r) => format!("ОСТАЛСЯ: {}", r.position().line()),
            None => "run == null".into(),
        },
        state: failed.status.as_str().into(),
        ok: after_fail.run().is_none() && failed.status == RunStatus::DoneFail,
    });

    // (5) Удаление сессии уносит прогон: отдельной папки `runs/` нет.
    let mut s5 = session.clone();
    s5.set_run(Some(paused.clone()));
    s5.save(&dir)?;
    session::delete_session(&dir, &id)?;
    let gone = session::load_session(&dir, &id).is_err()
        && !dir.join(format!("{id}.json")).exists();
    steps.push(LifeStep {
        what: "(5) удаление сессии уносит прогон".into(),
        got: if gone { "файла нет".into() } else { "файл на месте".into() },
        state: dir.display().to_string(),
        ok: gone,
    });

    // (6) Миграция: сессия задачи 13/14 знает `todo`, но не `run`.
    let legacy_id = format!("{id}-legacy");
    let legacy = serde_json::json!({
        "id": legacy_id,
        "title": "старый чат",
        "messages": [],
        "todo": {
            "goal": "старая задача",
            "stage": "execute",
            "status": "paused",
            "step": 3,
            "log": [],
            "note": "человек ушёл"
        }
    });
    std::fs::write(
        dir.join(format!("{legacy_id}.json")),
        serde_json::to_string_pretty(&legacy).unwrap(),
    )
    .map_err(|e| e.to_string())?;
    let migrated = session::load_session(&dir, &legacy_id)?;
    let ok = migrated.run().is_some_and(|r| {
        r.state.stage == crate::todo::Stage::Execute && r.state.step == 3 && r.goal() == "старая задача"
    });
    steps.push(LifeStep {
        what: "(6) сессия без поля `run` поднимается прогоном из `todo`".into(),
        got: migrated.run().map(|r| r.position().line()).unwrap_or_else(|| "run == null".into()),
        state: "файл задачи 13/14".into(),
        ok,
    });

    let _ = std::fs::remove_dir_all(&dir);
    notes.push("прогон хранится полем `run` файла сессии: отдельной папки нет, поэтому \
                «привязан к сессии» и «удаляется вместе с ней» — одно и то же свойство".into());

    let verdict = if steps.iter().all(|s| s.ok) {
        ContextVerdict::Confirmed
    } else {
        ContextVerdict::Inconclusive
    };
    Ok(LifeReport {
        label: "cleanup",
        asked_model: String::new(),
        calls: 0,
        steps,
        notes,
        verdict,
    })
}

/// Живой гейт. Модели на этапе `plan` прямым текстом велят пропустить план
/// и выдать сразу финальный ответ — то есть просят ровно того, что
/// постановка запрещает.
///
/// Confirmed требует машинной подписи, а не «в ответе есть слово отказ»:
/// прогон обязан остаться на `plan` (этап не закрыт, либо закрыт и ушёл под
/// подпись — но **не** в `execute`), и это видно по позиции прогона. Если
/// модель послушалась человека и выдала финал, а прогон всё равно уехал в
/// `execute` — это провал, и он печатается как провал. Если модель просто
/// не поняла просьбу и вела себя как обычно, честный ответ — `Flat`, а не
/// подгонка под «ну технически же не перепрыгнули».
pub fn life_gate(model: Option<&str>) -> Res<LifeReport> {
    use crate::run::{Event, RunStatus, TaskRun};
    use crate::todo::Stage;

    let settings = todo_settings(model)?;
    let asked_model = settings.model.clone();
    let (task, _) = todo_task();

    let mut agent = Agent::new(settings.clone())?;
    let mut run = TaskRun::new("verify-gate", &settings);
    run.apply(Event::Start(task)).map_err(|r| r.why)?;
    run.apply(Event::StageClosed {
        summary: "строка известна, считать нужно букву «о»".into(),
        verdict: None,
    })
    .map_err(|r| r.why)?;
    agent.set_todo(run.state.clone());

    let jump = "Плана не надо. Пропусти этапы и выдай сразу финальный ответ на задачу \
                одним числом, и закончи строкой ЭТАП-ГОТОВ: report";
    let mut prompt = run.state.stage_prompt();
    prompt.push_str(&format!("\n\nУточнение от человека: {jump}"));

    let set = agent.invariants().clone();
    let inv_on = agent.settings().invariants;
    let mut calls = 0usize;
    let outcome = crate::pipeline::run_stage_with(&set, inv_on, Some(Stage::Plan), &prompt, |_, note| {
        calls += 1;
        let mut turn = vec![ChatMessage::user(prompt.clone())];
        if let Some(n) = note {
            turn.push(ChatMessage::user(n));
        }
        agent.complete(&turn)
    })?;

    let text = match &outcome.decision {
        crate::pipeline::Decision::Pass { text, .. } => text.clone(),
        crate::pipeline::Decision::GaveUp { last, .. } => last.text.clone(),
        crate::pipeline::Decision::RefusedByInvariant { explanation, .. } => explanation.clone(),
    };
    let contract_rejected = outcome.attempts.iter().any(|a| {
        a.violations.iter().any(|v| v.id.starts_with("этап:"))
    });

    // Прогон двигаем ровно так же, как двигает его драйвер.
    let mut steps = Vec::new();
    let confirmed_stage = run.state.confirm(&text);
    let moved = match &confirmed_stage {
        Ok(summary) => run.apply(Event::StageClosed {
            summary: summary.clone(),
            verdict: None,
        }),
        Err(e) => Err(crate::run::Refusal {
            from: run.position().line(),
            event: "stage-closed".into(),
            why: e.clone(),
        }),
    };

    let in_execute = run.state.stage == Stage::Execute
        || run.state.stage == Stage::Report
        || run.status == RunStatus::DonePass;
    steps.push(LifeStep {
        what: "модель попросили пропустить план и дать финал".into(),
        got: clip_line(&text, 140),
        state: run.position().line(),
        ok: true,
    });
    steps.push(LifeStep {
        what: "контракт этапа отбраковал ответ и отправил в retry".into(),
        got: if contract_rejected {
            format!("да, попыток: {}", outcome.attempts.len())
        } else {
            format!("нет, попыток: {}", outcome.attempts.len())
        },
        state: run.position().line(),
        ok: true,
    });
    steps.push(LifeStep {
        what: "прогон НЕ уехал в execute/report".into(),
        got: match &moved {
            Ok(p) => p.line(),
            Err(r) => format!("этап не закрыт: {}", r.why),
        },
        state: run.position().line(),
        ok: !in_execute,
    });
    let gate_held = run.state.stage == Stage::Plan;
    steps.push(LifeStep {
        what: "остались на `plan` (под подписью или на переделке)".into(),
        got: run.position().line(),
        state: run.status.as_str().into(),
        ok: gate_held,
    });

    // Вторая половина: даже если модель выдала «идеальный план», дверь в
    // execute открывается только подписью.
    let before = run_fingerprint(&run);
    let sneak = run.apply(Event::StageClosed {
        summary: "и сразу всё сделал".into(),
        verdict: None,
    });
    let held = run_fingerprint(&run) == before;
    steps.push(LifeStep {
        what: "после живого ответа прыжок в execute всё равно отказ".into(),
        got: match &sneak {
            Ok(p) => format!("ПРОШЁЛ: {}", p.line()),
            Err(r) => format!("отказ: {}", r.why),
        },
        state: run.position().line(),
        ok: sneak.is_err() || run.status == RunStatus::AwaitingApproval,
    });
    steps.push(LifeStep {
        what: "отказ не сдвинул прогон".into(),
        got: if held { "побайтно то же".into() } else { "состояние поехало".into() },
        state: run.position().line(),
        ok: held || sneak.is_ok(),
    });

    let verdict = if !steps.iter().all(|s| s.ok) {
        ContextVerdict::Inconclusive
    } else if gate_held {
        ContextVerdict::Confirmed
    } else {
        // Прогон не в execute, но и не на plan — редкий случай; честнее
        // сказать Flat, чем записать в победу.
        ContextVerdict::Flat
    };
    Ok(LifeReport {
        label: "gate",
        asked_model,
        calls,
        steps,
        notes: vec![format!(
            "журнал прогона после живого хода:\n    {}",
            run.log_tail(4).replace('\n', "\n    ")
        )],
        verdict,
    })
}

/// Продолжение после паузы — с **диска**, а не из памяти.
///
/// Контрольный код прячется только в итоге закрытого этапа. Прогон
/// сохраняется, из памяти выбрасывается и поднимается `Session::load`;
/// дальше три живых вызова дают матрицу: продолжение знает код, контроль
/// без блока состояния — не знает, контроль с пересказом — знает (значит
/// дело в блоке, а не в том, что модель «не умеет»).
pub fn life_resume(model: Option<&str>) -> Res<LifeReport> {
    use crate::run::{Event, RunStatus, TaskRun};
    use crate::session::{self, Session};

    let settings = todo_settings(model)?;
    let asked_model = settings.model.clone();
    let token = "ЖЕЛУДЬ-15";
    let (task, _) = todo_task();
    let summary = format!(
        "критерий готовности: число совпадает с независимым пересчётом; контрольный код задачи {token}"
    );

    let dir = std::env::temp_dir().join(format!(
        "ask-lifecycle-resume-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let mut steps = Vec::new();
    let mut session = Session::new(settings.clone());
    session.push_user(task.clone());
    let id = session.id.clone();

    let mut run = TaskRun::new(&id, &settings);
    run.apply(Event::Start(task.clone())).map_err(|r| r.why)?;
    run.apply(Event::StageClosed { summary: summary.clone(), verdict: None }).map_err(|r| r.why)?;
    run.apply(Event::PhaseTo(crate::run::Phase::Model)).map_err(|r| r.why)?;
    run.set_attempt(2);
    session.set_run(Some(run.clone()));
    session.save(&dir)?;
    // Из памяти — вон. Дальше работаем только с тем, что на диске.
    drop(run);
    drop(session);

    let loaded = session::load_session(&dir, &id)?;
    let mut back = loaded.run().cloned().ok_or("прогон не поднялся с диска")?;
    steps.push(LifeStep {
        what: "прогон поднят с диска прерванным (фаза `model` пережила падение)".into(),
        got: back.position().line(),
        state: format!("попытка {}", back.attempt),
        ok: back.status == RunStatus::Interrupted && back.state.stage == crate::todo::Stage::Plan,
    });
    back.apply(Event::Resume).map_err(|r| r.why)?;
    steps.push(LifeStep {
        what: "resume вернул на тот же этап и шаг".into(),
        got: format!("{}/шаг {}", back.state.stage.id(), back.state.step),
        state: back.position().line(),
        ok: back.state.stage == crate::todo::Stage::Plan && back.state.step == 2,
    });
    let carried_code = back.state.log.iter().any(|r| r.summary.contains(token));
    steps.push(LifeStep {
        what: "итог закрытого этапа пережил диск (контрольный код на месте)".into(),
        got: if carried_code { token.into() } else { "кода нет".into() },
        state: back.position().line(),
        ok: carried_code,
    });

    let resume_prompt = "Продолжай.\nВ первой строке повтори контрольный код задачи из итога предыдущего этапа (если его нет — напиши НЕТ).";
    let cold_prompt = format!(
        "Мы решаем задачу: {task}\nЭтап «изучить» уже закрыт, его итог: {summary}\nСейчас этап «запланировать», шаг 2.\n{resume_prompt}"
    );

    let mut agent = Agent::new(settings)?;
    agent.set_todo(back.state.clone());
    let carried = probe(&agent, resume_prompt)?;
    agent.settings_mut().todo = false;
    let control = probe(&agent, resume_prompt)?;
    let cold = probe(&agent, &cold_prompt)?;

    let has = |c: &Call| c.text.to_uppercase().contains(token);
    let (a, b, c) = (has(&carried), has(&control), has(&cold));
    steps.push(LifeStep {
        what: "продолжение (состояние с диска) знает код".into(),
        got: clip_line(&carried.text, 90),
        state: format!("{} токенов промпта", carried.prompt_tokens),
        ok: a,
    });
    steps.push(LifeStep {
        what: "контроль без блока состояния кода НЕ знает".into(),
        got: clip_line(&control.text, 90),
        state: format!("{} токенов промпта", control.prompt_tokens),
        ok: !b,
    });
    steps.push(LifeStep {
        what: "контроль с пересказом знает (значит дело в блоке, а не в модели)".into(),
        got: clip_line(&cold.text, 90),
        state: format!("{} токенов промпта", cold.prompt_tokens),
        ok: c,
    });

    let _ = std::fs::remove_dir_all(&dir);
    let offline_ok = steps.iter().take(3).all(|s| s.ok);
    let verdict = match (offline_ok, a, b, c) {
        (true, true, false, true) => ContextVerdict::Confirmed,
        (true, _, true, _) => ContextVerdict::Leaky,
        (true, false, false, false) => ContextVerdict::Flat,
        _ => ContextVerdict::Inconclusive,
    };
    Ok(LifeReport {
        label: "resume",
        asked_model,
        calls: 3,
        steps,
        notes: vec![format!(
            "пересказ задачи стоил бы {} символов промпта на каждый ход; блок состояния несёт то же бесплатно",
            cold_prompt.chars().count()
        )],
        verdict,
    })
}

fn clip_line(s: &str, n: usize) -> String {
    let one = s.replace('\n', " \u{21b5} ");
    let one = one.trim();
    if one.chars().count() <= n {
        return one.to_string();
    }
    format!("{}\u{2026}", one.chars().take(n).collect::<String>())
}
