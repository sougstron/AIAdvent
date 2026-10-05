//! Task 25: task memory for the RAG chat («память задачи»).
//!
//! The mini-chat (`ask --rag-chat`, and the TUI while `rag` is on) keeps,
//! besides the raw dialogue history, a structured state of *what this
//! conversation is about*: the goal, what the user has already clarified,
//! the constraints that were fixed and the agreed terms. That state is what
//! keeps a 10–15-message dialogue on the rails: a short late question like
//! «а таймауты?» is still answered inside the original task, because the
//! goal and the constraints ride every request.
//!
//! The state is used twice per turn, both times deterministically:
//!
//! * **retrieval** — when it is non-empty, the embedding query is the
//!   question plus one line with the goal and the constraints, so a terse
//!   follow-up still lands on the right documents;
//! * **generation** — the same state goes into the grounded-answer prompt
//!   (`cite.rs`) as one `<task-state>` block with exact tag boundaries, so
//!   the answer respects the goal and the fixed constraints.
//!
//! The state itself is maintained by an LLM **extractor** at temperature 0:
//! after every answered turn it gets the current state (JSON), the user's
//! message and the answer, and returns the updated JSON. The model proposes,
//! the code disposes: `parse_state` validates the schema, trims, dedupes and
//! caps every list, and a reply that does not parse is dropped — the old
//! state survives, an extractor failure can never erase memory.
//!
//! The state lives in the session file (`Session::task_mem`): it survives
//! Esc, app exit and restart, and is deleted together with the session.
//! `ask --rag-chat-eval` proves the whole loop on two scripted scenarios of
//! 12–14 messages each → `rag/chat.md`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Write as _;
use std::path::Path;

use crate::agent::Agent;
use crate::api::{ChatMessage, Usage};
use crate::cite::{self, Card, Checker, Idk, Status};
use crate::config::{Res, Settings};
use crate::ragqa::{self, Retriever};
use crate::rerank::{self, Judge, Pipeline};

/// Caps the extractor cannot exceed, no matter what it returns. Short items
/// keep the `<task-state>` block cheap on every request.
const MAX_CLARIFIED: usize = 8;
const MAX_CONSTRAINTS: usize = 8;
const MAX_TERMS: usize = 10;
const MAX_ITEM: usize = 200;
const MAX_GOAL: usize = 300;

// ---------------------------------------------------------------- the state

/// A term with the meaning this dialogue fixed for it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Term {
    pub term: String,
    pub meaning: String,
}

/// «Память задачи»: цель диалога, что пользователь уже уточнил, какие
/// ограничения и термины зафиксированы. `Default` — диалог ещё ни о чём.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskMem {
    /// The overall goal of the dialogue, one sentence. Set from the first
    /// messages; changes only when the user explicitly changes the objective.
    #[serde(default)]
    pub goal: String,
    /// What the user has clarified about the task so far.
    #[serde(default)]
    pub clarified: Vec<String>,
    /// Fixed constraints (budget, latency, stack, sizes…). Never dropped
    /// unless the user cancels one.
    #[serde(default)]
    pub constraints: Vec<String>,
    /// Terms with the meaning agreed in this dialogue.
    #[serde(default)]
    pub terms: Vec<Term>,
}

impl TaskMem {
    pub fn is_empty(&self) -> bool {
        self.goal.is_empty() && self.clarified.is_empty() && self.constraints.is_empty() && self.terms.is_empty()
    }

    /// The block that rides every grounded-answer request while the memory
    /// is on and non-empty. Tag-delimited so the boundary is exact (the same
    /// trick as the profile's `<user-profile>`: the rest of the prompt must
    /// stay byte-identical when the state changes). `None` — nothing to add.
    pub fn block(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut s = String::from("<task-state>\n");
        if !self.goal.is_empty() {
            s += &format!("Goal of this dialogue: {}\n", self.goal);
        }
        for x in &self.clarified {
            s += &format!("User clarified: {x}\n");
        }
        for x in &self.constraints {
            s += &format!("Fixed constraint: {x}\n");
        }
        for t in &self.terms {
            s += &format!("Agreed term: {} = {}\n", t.term, t.meaning);
        }
        s += "Answer inside this task: a short or pronoun-only question refers to the goal above; never violate a fixed constraint.\n</task-state>\n\n";
        Some(s)
    }

    /// The retrieval query for this turn: the question itself plus, while
    /// the memory has anything to say, one context line. Without it a terse
    /// follow-up («а таймауты?») embeds far from the documents the dialogue
    /// is actually about.
    pub fn retrieval_query(&self, question: &str) -> String {
        if self.is_empty() {
            return question.to_string();
        }
        let mut ctx = self.goal.clone();
        if !self.constraints.is_empty() {
            ctx += &format!("; ограничения: {}", self.constraints.join(", "));
        }
        if !self.terms.is_empty() {
            ctx += &format!("; термины: {}", self.terms.iter().map(|t| t.term.as_str()).collect::<Vec<_>>().join(", "));
        }
        if ctx.is_empty() {
            return question.to_string();
        }
        format!("{}\n(контекст задачи: {})", question.trim(), ctx)
    }

    /// Human-readable card (`/chatmem`, `--rag-chat` command `/state`).
    pub fn card(&self) -> String {
        if self.is_empty() {
            return "память задачи пуста — состояние появится после первого отвеченного хода".into();
        }
        let mut s = format!("цель: {}\n", if self.goal.is_empty() { "—".into() } else { self.goal.clone() });
        s += &format!("уточнено ({}): {}\n", self.clarified.len(), if self.clarified.is_empty() { "—".into() } else { self.clarified.join(" · ") });
        s += &format!(
            "ограничения ({}): {}\n",
            self.constraints.len(),
            if self.constraints.is_empty() { "—".into() } else { self.constraints.join(" · ") }
        );
        s += &format!(
            "термины ({}): {}",
            self.terms.len(),
            if self.terms.is_empty() { "—".into() } else { self.terms.iter().map(|t| format!("{} = {}", t.term, t.meaning)).collect::<Vec<_>>().join(" · ") }
        );
        s
    }
}

/// The extractor's reply as the model sent it.
#[derive(Debug, Default, Deserialize)]
struct RawMem {
    #[serde(default)]
    goal: String,
    #[serde(default)]
    clarified: Vec<String>,
    #[serde(default)]
    constraints: Vec<String>,
    #[serde(default)]
    terms: Vec<Term>,
}

fn clean(item: &str) -> String {
    item.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(MAX_ITEM).collect()
}

fn dedupe(items: Vec<String>, cap: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for x in items.into_iter().map(|x| clean(&x)).filter(|x| !x.is_empty()) {
        // eq_ignore_ascii_case — только про ASCII; дубликаты «Порог»/«порог»
        // сливаем сравнением в нижнем регистре.
        if !out.iter().any(|o| o.to_lowercase() == x.to_lowercase()) {
            out.push(x);
        }
        if out.len() == cap {
            break;
        }
    }
    out
}

/// The model proposes, the code disposes: JSON somewhere in the reply → a
/// validated, capped `TaskMem`. `None` — the reply is unusable and the
/// caller keeps the previous state.
pub fn parse_state(text: &str) -> Option<TaskMem> {
    let (a, b) = text.find('{').zip(text.rfind('}'))?;
    let raw: RawMem = serde_json::from_str(text.get(a..=b)?).ok()?;
    let mut terms: Vec<Term> = Vec::new();
    for t in raw.terms {
        let (term, meaning) = (clean(&t.term), clean(&t.meaning));
        if !term.is_empty() && !terms.iter().any(|o| o.term.to_lowercase() == term.to_lowercase()) {
            terms.push(Term { term, meaning });
        }
        if terms.len() == MAX_TERMS {
            break;
        }
    }
    Some(TaskMem {
        goal: raw.goal.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(MAX_GOAL).collect(),
        clarified: dedupe(raw.clarified, MAX_CLARIFIED),
        constraints: dedupe(raw.constraints, MAX_CONSTRAINTS),
        terms,
    })
}

// ---------------------------------------------------------------- extractor

/// The «what is this dialogue about» extractor: the user's model at
/// temperature 0, no system extras — the same eval-agent shape as the
/// citation judge (`cite::Checker`).
pub struct Extractor {
    agent: Agent,
}

impl Extractor {
    pub fn new(settings: &Settings) -> Res<Extractor> {
        let mut agent = ragqa::eval_agent(settings)?;
        agent.settings_mut().temperature = Some(0.0);
        agent.settings_mut().system_prompt = "You maintain the structured task memory of a conversation. Follow the output format exactly.".into();
        Ok(Extractor { agent })
    }

    /// One update step: current state + the last exchange → the updated
    /// state. `Err` (network or unparseable reply) never loses memory — the
    /// caller falls back to the state it passed in.
    pub fn update(&self, mem: &TaskMem, question: &str, answer: &str) -> Res<(TaskMem, Usage)> {
        let p = format!(
            "Below is the task memory of a dialogue between a user and a RAG assistant, the user's last message and \
             the assistant's answer. Return the UPDATED memory as ONE JSON object and nothing else:\n\
             {{\"goal\":\"…\",\"clarified\":[\"…\"],\"constraints\":[\"…\"],\"terms\":[{{\"term\":\"…\",\"meaning\":\"…\"}}]}}\n\
             Rules:\n\
             - goal: the user's overall objective in this dialogue, one sentence, in the user's language. Set it from \
             the first messages; change it only when the user explicitly changes the objective.\n\
             - clarified: what the user has clarified so far — about the task and about themselves and their \
             situation (name, what they own or work on, background) — short items, user's language. Add new ones, \
             do not repeat.\n\
             - constraints: fixed requirements the user stated (sizes, budget, latency, stack, format…). Never drop \
             one unless the user explicitly cancels it.\n\
             - terms: terms this dialogue gave an agreed meaning to — the meaning must be stated by the user or by \
             the assistant's answer; never fill a meaning from your own knowledge.\n\
             - The assistant may have had no answer (shown as «(no answer)») — the user's statements still count.\n\
             - Keep every item under 200 characters; at most {MAX_CLARIFIED} clarified, {MAX_CONSTRAINTS} constraints, \
             {MAX_TERMS} terms.\n\n\
             Current memory:\n{}\n\nUser: {}\n\nAssistant: {}",
            serde_json::to_string(mem).unwrap_or_default(),
            question.trim(),
            if answer.trim().is_empty() { "(no answer)" } else { answer.trim() }
        );
        let o = self.agent.complete_outcome(&[ChatMessage::user(p)])?;
        let text = o.text();
        let mem = parse_state(text)
            .ok_or_else(|| format!("экстрактор вернул не JSON: {}", text.chars().take(160).collect::<String>()))?;
        Ok((mem, o.usage))
    }
}

// ---------------------------------------------------------------- recall

/// Маркер отказа: ответа в памяти и истории нет.
const NOT_IN_MEMORY: &str = "NOT-IN-MEMORY";

/// Записи памяти диалога, на которые может сослаться ответ из памяти:
/// пункты состояния задачи и реплики пользователя из истории, по порядку.
/// Номер записи в промпте — `[M{i+1}]`.
pub fn memory_entries(mem: &TaskMem, history: &[ChatMessage]) -> Vec<(String, String)> {
    let mut out = vec![];
    if !mem.goal.is_empty() {
        out.push(("цель".to_string(), mem.goal.clone()));
    }
    out.extend(mem.clarified.iter().map(|x| ("уточнено".to_string(), x.clone())));
    out.extend(mem.constraints.iter().map(|x| ("ограничение".to_string(), x.clone())));
    out.extend(mem.terms.iter().map(|t| ("термин".to_string(), format!("{} = {}", t.term, t.meaning))));
    let users = history.iter().filter(|m| m.role == crate::api::Role::User);
    out.extend(users.enumerate().map(|(k, m)| (format!("реплика пользователя №{}", k + 1), clean(&m.content))));
    out
}

/// The recall prompt: the numbered memory entries plus a strict «только из
/// памяти, со ссылкой на запись и дословной цитатой» contract. Отдельная
/// чистая функция — чтобы офлайн-тест видел контракт и нумерацию.
pub fn recall_prompt(question: &str, entries: &[(String, String)]) -> String {
    let list: String = entries.iter().enumerate().map(|(i, (kind, text))| format!("[M{}] {kind}: {text}\n", i + 1)).collect();
    format!(
        "Answer the question using ONLY the numbered memory entries of this dialogue below (the task memory and \
         the user's own messages). The question is about what THIS dialogue agreed on or what the user told in it \
         (the goal, the fixed constraints, the terms, the clarifications, facts about the user) — not about the \
         documents. Never add facts from your own knowledge. Reply with ONE JSON object and nothing else:\n\
         {{\"answer\":\"…\",\"sources\":[{{\"n\":1,\"quote\":\"…\"}}]}}\n\
         - answer: in the language of the question, short and to the point.\n\
         - sources: every entry the answer relies on — n is the entry number (M1 → 1), quote copies the supporting \
         words of THAT entry verbatim.\n\
         If the answer is not in the entries, reply with exactly: {NOT_IN_MEMORY}\n\n\
         Memory entries:\n{list}\nQuestion: {}",
        question.trim()
    )
}

#[derive(Deserialize)]
struct RawRecall {
    #[serde(default)]
    answer: String,
    #[serde(default)]
    sources: Vec<RawMemRef>,
}

#[derive(Deserialize)]
struct RawMemRef {
    #[serde(default)]
    n: Value,
    #[serde(default)]
    quote: String,
}

/// Минимум букв/цифр в цитате из памяти: записи памяти короткие («зовут
/// Шурик»), порог корпусных цитат тут не подходит, но одна-две буквы не
/// доказывают ничего.
const MIN_MEM_QUOTE: usize = 3;

/// The model proposes, the code disposes: the recall reply → the answer and
/// the memory sources it cited, each quote checked against its entry.
/// Ссылки на несуществующие записи отбрасываются. Ответ не в JSON — текст
/// целиком как ответ без источников (карточка покажет «НЕ подтверждён»).
pub fn parse_recall(text: &str, entries: &[(String, String)]) -> (String, Vec<cite::MemSource>) {
    let raw = text.find('{').zip(text.rfind('}')).and_then(|(a, b)| serde_json::from_str::<RawRecall>(text.get(a..=b)?).ok());
    let Some(raw) = raw.filter(|r| !r.answer.trim().is_empty()) else { return (text.trim().to_string(), vec![]) };
    let mut out: Vec<cite::MemSource> = vec![];
    for r in raw.sources {
        let n = match &r.n {
            Value::Number(x) => x.as_u64().map(|x| x as usize),
            Value::String(x) => x.trim().trim_start_matches(['M', 'm', 'М', 'м']).parse().ok(),
            _ => None,
        };
        let Some((n, (kind, entry))) = n.and_then(|n| Some((n, entries.get(n.checked_sub(1)?)?))) else { continue };
        if out.iter().any(|m| m.n == n) {
            continue;
        }
        let q = cite::compact(&r.quote);
        let quote_ok = q.chars().count() >= MIN_MEM_QUOTE && cite::compact(entry).contains(&q);
        out.push(cite::MemSource { n, kind: kind.clone(), text: entry.clone(), quote: clean(&r.quote), quote_ok });
    }
    (raw.answer.trim().to_string(), out)
}

/// Ответ «из памяти задачи»: вопрос про договорённости, а не про корпус.
/// Источники ответа — записи памяти диалога (`memory_entries`) с цитатами.
/// `Ok(None)` — модель честно сказала, что в памяти этого нет: вызывающий
/// оставляет исходное «не знаю». Та же модель, что у ответов; температура 0,
/// чтобы пересказ состояния был буквальным.
pub fn recall(agent: &Agent, history: &[ChatMessage], mem: &TaskMem, question: &str) -> Res<Option<Card>> {
    let entries = memory_entries(mem, history);
    if entries.is_empty() {
        return Ok(None);
    }
    let mut wire = history.to_vec();
    wire.push(ChatMessage::user(recall_prompt(question, &entries)));
    // Пересказ зафиксированного — задача буквальная, температура 0.
    let mut agent = agent.clone();
    agent.settings_mut().temperature = Some(0.0);
    let start = std::time::Instant::now();
    let o = agent.complete_outcome(&wire)?;
    let text = o.text().trim().to_string();
    if text.contains(NOT_IN_MEMORY) {
        return Ok(None);
    }
    let (answer, sources) = parse_recall(&text, &entries);
    Ok(Some(Card::recall(&answer, sources, &o.usage, start.elapsed().as_millis())))
}

// ---------------------------------------------------------------- the chat

/// One turn of the mini-chat, for the report and the REPL status line.
pub struct Turn {
    pub card: Card,
    /// The retrieval query actually embedded (the question plus the memory
    /// line while the state is non-empty).
    pub query: String,
    /// The transcript note naming the chunks (`ragqa::sources_note`).
    pub note: String,
    /// The chunks that made the context, one line each (`ragqa::hit_line`).
    pub context: Vec<String>,
    /// Extractor failure, if any — the turn still counts, the state just
    /// did not move.
    pub mem_error: Option<String>,
    /// Tokens of the retrieval stages (rewrite / reranker).
    pub stage_tokens: u64,
    /// Tokens of the extractor call.
    pub mem_tokens: u64,
}

/// The mini-chat itself: history + retrieval on every question + grounded
/// answers with sources + the task memory. `ask --rag-chat` talks to it
/// interactively, `ask --rag-chat-eval` replays scenarios through the exact
/// same [`Chat::turn`], so the check is the product and not a copy of it.
pub struct Chat {
    settings: Settings,
    pipeline: Pipeline,
    agent: Agent,
    checker: Option<Checker>,
    extractor: Extractor,
    retriever: Retriever,
    judge: Option<Judge>,
    idk: Idk,
    /// Raw dialogue: user questions as typed, assistant answers as the
    /// printed cards. Retrieval prompts never land here.
    pub history: Vec<ChatMessage>,
    pub mem: TaskMem,
}

impl Chat {
    pub fn new(settings: &Settings, paths: &crate::rag::Config) -> Res<Chat> {
        let pipeline = Pipeline::from_settings(settings);
        let retriever = Retriever::open(&paths.db, &settings.rag_strategy, &paths.url)?;
        let judge = if pipeline.needs_llm() { Some(Judge::new(settings, &retriever)?) } else { None };
        let checker = Checker::new(settings).ok();
        Ok(Chat {
            settings: settings.clone(),
            pipeline,
            agent: ragqa::eval_agent(settings)?,
            checker,
            extractor: Extractor::new(settings)?,
            retriever,
            judge,
            idk: Idk::from_settings(settings),
            history: Vec::new(),
            mem: TaskMem::default(),
        })
    }

    /// One turn. Retrieval sees the question plus the memory line; the model
    /// sees the `<task-state>` block; after every turn the extractor
    /// moves the state — «не знаю» included: the user's message may still
    /// carry the goal or facts about themselves. Особый
    /// случай: вопрос был о самих договорённостях («напомни, какой реранкер
    /// мы зафиксировали») — корпуса под ним нет по определению, поэтому порог
    /// релевантности честно отвечает «не знаю». Тогда, при непустой памяти или истории,
    /// срабатывает [`recall`]: ответ строится из
    /// состояния и истории, с записями памяти диалога как источниками — вместо
    /// потери цели диалога.
    pub fn turn(&mut self, question: &str) -> Res<Turn> {
        let mem_on = self.settings.chatmem;
        let query = if mem_on { self.mem.retrieval_query(question) } else { question.to_string() };
        let got = rerank::run(&self.retriever, &self.pipeline, self.judge.as_ref(), &query)?;
        let note = ragqa::sources_note(&self.retriever, &self.pipeline, &got, 0);
        let mem = if mem_on { Some(&self.mem) } else { None };
        let mut wire = self.history.clone();
        wire.push(ChatMessage::user(question));
        let mut card = cite::answer(&self.agent, self.checker.as_ref(), &wire, question, &got, self.idk, mem)?;
        let stage_tokens = got.prompt_tokens + got.completion_tokens;
        let context = got.kept.iter().map(ragqa::hit_line).collect();
        if mem_on && card.status == Status::Unknown && !(self.mem.is_empty() && self.history.is_empty()) {
            // Отказ recall (сеть, модель ответила NOT-IN-MEMORY) оставляет
            // честное «не знаю» — он строго безопаснее выдумки.
            if let Ok(Some(rc)) = recall(&self.agent, &self.history, &self.mem, question) {
                card = rc;
            }
        }
        let mut mem_error = None;
        let mut mem_tokens = 0;
        // Ход «из памяти» сам память не двигает: в нём нет ничего нового,
        // он только пересказывает уже зафиксированное. «Не знаю» — двигает:
        // ответа в корпусе нет, но пользователь мог сказать о цели или о
        // себе («меня зовут Шурик»), и это должно дожить до следующего хода.
        if mem_on && !card.from_memory {
            // «Не знаю» ничего не утверждает — экстрактору только слова пользователя.
            let answer = if card.status == Status::Answer { card.answer.as_str() } else { "" };
            match self.extractor.update(&self.mem, question, answer) {
                Ok((new, u)) => {
                    self.mem = new;
                    mem_tokens = u.prompt_tokens + u.completion_tokens;
                }
                Err(e) => mem_error = Some(e),
            }
        }
        self.history.push(ChatMessage::user(question));
        self.history.push(ChatMessage::assistant(card.to_text()));
        Ok(Turn { card, query, note, context, mem_error, stage_tokens, mem_tokens })
    }
}

/// `ask --rag-chat`: the interactive mini-chat. One line — one turn; the
/// answer always comes with sources. `/state`, `/reset`, `/mem on|off`,
/// `/quit` are the only commands; Ctrl-D ends too.
pub fn repl(settings: &Settings, paths: &crate::rag::Config) -> Res<()> {
    let mut chat = Chat::new(settings, paths)?;
    println!(
        "RAG-чат · модель {} · индекс {} ({} чанков) · память задачи {}\n\
         каждый ответ — с источниками; /state — память задачи, /reset — забыть, /mem on|off, /quit — выход",
        settings.model,
        paths.db.display(),
        chat.retriever.len(),
        if settings.chatmem { "on" } else { "off" }
    );
    let stdin = std::io::stdin();
    loop {
        print!("\n> ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        if stdin.read_line(&mut line).unwrap_or(0) == 0 {
            println!();
            return Ok(());
        }
        let q = line.trim();
        if q.is_empty() {
            continue;
        }
        match q {
            "/quit" | "/exit" => return Ok(()),
            "/state" => {
                println!("{}", chat.mem.card());
                continue;
            }
            "/reset" => {
                chat.mem = TaskMem::default();
                println!("память задачи очищена (история диалога осталась)");
                continue;
            }
            "/mem on" | "/mem off" => {
                chat.settings.chatmem = q == "/mem on";
                println!("память задачи {}", if chat.settings.chatmem { "on" } else { "off" });
                continue;
            }
            _ if q.starts_with('/') => {
                println!("команды: /state, /reset, /mem on|off, /quit");
                continue;
            }
            _ => {}
        }
        match chat.turn(q) {
            Ok(t) => {
                println!("{}", t.note);
                if t.query.trim() != q {
                    eprintln!("· запрос эмбеддинга: {}", t.query.replace('\n', " ↲ "));
                }
                println!("{}", t.card.to_text());
                if let Some(e) = t.mem_error {
                    eprintln!("· память задачи не обновилась: {e}");
                }
                eprintln!(
                    "· попыток {} · {} tok ответа · {} tok этапов · {} tok памяти",
                    t.card.attempts,
                    t.card.prompt_tokens + t.card.completion_tokens,
                    t.stage_tokens,
                    t.mem_tokens
                );
            }
            Err(e) => eprintln!("error: {e}"),
        }
    }
}

// ---------------------------------------------------------------- eval

/// A scripted scenario (`docs/chat-scenarios.json`): one long dialogue with
/// machine-checkable expectations per turn.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Scenario {
    pub id: usize,
    pub title: String,
    /// What the state should name as the goal, in words — for the report.
    pub goal: String,
    /// Groups (the `alt|alt` format of `ragqa::covers`) that must appear in
    /// the extracted goal by the end of the scenario.
    #[serde(default)]
    pub goal_has: Vec<String>,
    #[serde(default)]
    pub turns: Vec<ChatTurn>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatTurn {
    pub q: String,
    /// `answer` — the corpus holds it, sources are mandatory; `unknown` —
    /// it does not, and «не знаю» is the only right outcome.
    #[serde(default = "default_expect")]
    pub expect: String,
    /// Every group must appear in the answer (goal/constraint retention is
    /// checked the same way — the late turns' groups name the fixed facts).
    #[serde(default)]
    pub must: Vec<String>,
    /// Groups that betray an answer made up off-corpus.
    #[serde(default)]
    pub wrong: Vec<String>,
    /// Substrings (case-insensitive) the task memory must hold after this
    /// turn — «the constraint fixed at turn 3 is still there at turn 12».
    #[serde(default)]
    pub state_has: Vec<String>,
}

fn default_expect() -> String {
    "answer".into()
}

pub fn load_scenarios(dir: &Path) -> Res<Vec<Scenario>> {
    let path = dir.join("chat-scenarios.json");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_value(v["scenarios"].clone()).map_err(|e| format!("{}: scenarios: {e}", path.display()))
}

#[derive(Debug, Serialize)]
pub struct TurnRow {
    pub n: usize,
    pub q: String,
    pub expect: String,
    pub status: String,
    pub sources: usize,
    pub sources_ok: usize,
    pub quotes_ok: bool,
    pub numbers_ok: bool,
    pub judge: String,
    pub covered: usize,
    pub must: usize,
    pub made_up: Vec<String>,
    /// How many `state_has` checks this turn declared.
    pub state_checks: usize,
    pub state_missing: Vec<String>,
    pub context: Vec<String>,
    pub card: Option<Card>,
    pub mem_error: Option<String>,
    pub error: Option<String>,
    pub tokens: u64,
}

#[derive(Debug, Serialize)]
pub struct ScenarioRow {
    pub id: usize,
    pub title: String,
    pub goal: String,
    pub goal_text: String,
    pub goal_covered: usize,
    pub goal_must: usize,
    pub turns: Vec<TurnRow>,
    pub mem: TaskMem,
}
fn judge_label(card: &Card) -> String {
    card.support.as_ref().map(|s| s.verdict.clone()).unwrap_or_else(|| "—".into())
}

/// `ask --rag-chat-eval`: both scenarios through the same [`Chat::turn`]
/// the REPL uses → `rag/chat.md` + `rag/chat.json`. The check is causal in
/// the same sense as the earlier verifiers: sources/quotes/numbers are
/// checked against the chunks, goal retention is checked by word groups the
/// answer must contain, the memory is checked by reading the state itself.
pub fn eval(settings: &Settings, paths: &crate::rag::Config, only: &[usize]) -> Res<()> {
    let scenarios = load_scenarios(&paths.dir)?;
    let mut rows: Vec<ScenarioRow> = Vec::new();
    for sc in scenarios.iter().filter(|s| only.is_empty() || only.contains(&s.id)) {
        println!("\n=== сценарий {}: {} ({} ходов) ===", sc.id, sc.title, sc.turns.len());
        let mut chat = Chat::new(settings, paths)?;
        let mut trows: Vec<TurnRow> = Vec::new();
        for (i, t) in sc.turns.iter().enumerate() {
            println!("[{}.{:02}] {}", sc.id, i + 1, t.q);
            let (turn, error) = match chat.turn(&t.q) {
                Ok(t) => (Some(t), None),
                Err(e) => (None, Some(e)),
            };
            let row = match turn {
                Some(turn) => {
                    let c = &turn.card;
                    let text = c.answer.clone();
                    let state_json = serde_json::to_string(&chat.mem).unwrap_or_default().to_lowercase();
                    let state_missing: Vec<String> =
                        t.state_has.iter().filter(|g| !g.to_lowercase().split('|').any(|a| state_json.contains(a.trim()))).cloned().collect();
                    let made_up: Vec<String> = t.wrong.iter().filter(|g| ragqa::covers(&text, g)).cloned().collect();
                    let row = TurnRow {
                        n: i + 1,
                        q: t.q.clone(),
                        expect: t.expect.clone(),
                        status: match c.status {
                            Status::Answer => "answer".into(),
                            Status::Unknown => "unknown".into(),
                        },
                        // ответ из памяти: источники — записи памяти диалога
                        sources: if c.from_memory { c.mem_sources.len() } else { c.sources.len() },
                        sources_ok: if c.from_memory { c.mem_sources.iter().filter(|m| m.quote_ok).count() } else { c.sources.iter().filter(|s| s.id_ok).count() },
                        quotes_ok: c.check.as_ref().is_some_and(|k| k.quotes > 0 && k.quotes_ok == k.quotes),
                        numbers_ok: c.check.as_ref().is_some_and(|k| k.unbacked.is_empty()),
                        judge: judge_label(c),
                        covered: ragqa::coverage(&text, &t.must).0,
                        must: t.must.len(),
                        made_up,
                        state_checks: t.state_has.len(),
                        state_missing,
                        context: turn.context.clone(),
                        card: Some(turn.card),
                        mem_error: turn.mem_error,
                        error: None,
                        tokens: turn.stage_tokens + turn.mem_tokens,
                    };
                    println!("    {}", turn_progress(&row));
                    row
                }
                None => {
                    println!("    ОШИБКА: {}", error.as_deref().unwrap_or(""));
                    TurnRow {
                        n: i + 1,
                        q: t.q.clone(),
                        expect: t.expect.clone(),
                        status: "error".into(),
                        sources: 0,
                        sources_ok: 0,
                        quotes_ok: false,
                        numbers_ok: false,
                        judge: "—".into(),
                        covered: 0,
                        must: t.must.len(),
                        made_up: vec![],
                        state_checks: t.state_has.len(),
                        state_missing: vec![],
                        context: vec![],
                        card: None,
                        mem_error: None,
                        error,
                        tokens: 0,
                    }
                }
            };
            trows.push(row);
        }
        let goal_covered = ragqa::coverage(&chat.mem.goal, &sc.goal_has).0;
        println!("цель в памяти: «{}» ({}/{})", chat.mem.goal, goal_covered, sc.goal_has.len());
        rows.push(ScenarioRow {
            id: sc.id,
            title: sc.title.clone(),
            goal: sc.goal.clone(),
            goal_text: chat.mem.goal.clone(),
            goal_covered,
            goal_must: sc.goal_has.len(),
            turns: trows,
            mem: chat.mem.clone(),
        });
    }
    let dir = paths.db.parent().map(Path::to_path_buf).unwrap_or_default();
    let (md_path, json_path) = (dir.join("chat.md"), dir.join("chat.json"));
    let md = report(settings, paths, &rows);
    std::fs::write(&md_path, &md).map_err(|e| format!("{}: {e}", md_path.display()))?;
    let raw = json!({"model": settings.model, "scenarios": rows});
    std::fs::write(&json_path, serde_json::to_string_pretty(&raw).unwrap_or_default()).map_err(|e| format!("{}: {e}", json_path.display()))?;
    println!("\n{}", summary(&rows));
    println!("отчёт: {} (+ {})", md_path.display(), json_path.display());
    Ok(())
}

fn turn_progress(row: &TurnRow) -> String {
    if let Some(e) = &row.error {
        return format!("ОШИБКА: {e}");
    }
    let mut s = format!("статус {} (ждали {})", row.status, row.expect);
    if row.card.as_ref().is_some_and(|c| c.from_memory) {
        s += " · из памяти задачи";
    }
    if row.expect == "answer" {
        s += &format!(" · источники {}/{}", row.sources_ok, row.sources);
        if row.must > 0 {
            s += &format!(" · ожидание {}/{}", row.covered, row.must);
        }
    }
    if !row.made_up.is_empty() {
        s += &format!(" · ВЫДУМАЛ: {}", row.made_up.join(", "));
    }
    if !row.state_missing.is_empty() {
        s += &format!(" · память без: {}", row.state_missing.join(", "));
    }
    s
}

/// One line per metric, per scenario — the «does it hold up» table.
pub fn summary(rows: &[ScenarioRow]) -> String {
    let mut md = String::from("| сценарий | ходов | ответов с источниками | цитаты дословно | числа в цитатах | судья: да | «не знаю» правильно | ожидание must | выдумал | память: проверок прошло | цель поймана |\n|---|---|---|---|---|---|---|---|---|---|---|\n");
    for sc in rows {
        let answered: Vec<&TurnRow> = sc.turns.iter().filter(|t| t.expect == "answer" && t.status == "answer").collect();
        let n = answered.len();
        let count = |f: &dyn Fn(&TurnRow) -> bool| answered.iter().filter(|t| f(t)).count();
        let unknown: Vec<&TurnRow> = sc.turns.iter().filter(|t| t.expect == "unknown").collect();
        let unknown_ok = unknown.iter().filter(|t| t.status == "unknown").count();
        let (must_sum, must_got) =
            sc.turns.iter().filter(|t| t.expect == "answer").fold((0, 0), |(m, g), t| (m + t.must, g + t.covered));
        let made_up = sc.turns.iter().filter(|t| !t.made_up.is_empty()).count();
        let (state_sum, state_got) = sc.turns.iter().fold((0, 0), |(m, g), t| (m + t.state_checks, g + t.state_checks - t.state_missing.len()));
        md += &format!(
            "| {}. {} | {} | {}/{n} | {}/{n} | {}/{n} | {}/{n} | {}/{} | {must_got}/{must_sum} | {} | {}/{} | {}/{} |\n",
            sc.id,
            sc.title,
            sc.turns.len(),
            count(&|t| t.sources > 0),
            count(&|t| t.quotes_ok),
            count(&|t| t.numbers_ok),
            count(&|t| t.judge == "yes"),
            unknown_ok,
            unknown.len(),
            made_up,
            state_got,
            state_sum,
            sc.goal_covered,
            sc.goal_must,
        );
    }
    md
}


fn report(settings: &Settings, paths: &crate::rag::Config, rows: &[ScenarioRow]) -> String {
    let mut md = format!(
        "# RAG-чат с памятью задачи (задача 25)\n\n\
         Модель `{}`, индекс `{}` (стратегия `{}`), память задачи {}. Каждый ход: вопрос (+ строка состояния в \
         запросе эмбеддинга) → retrieval → ответ с обязательными источниками и дословными цитатами → экстрактор \
         обновляет память задачи (цель / уточнено / ограничения / термины), которая блоком `<task-state>` едет в \
         каждый следующий запрос.\n\n## Сводка\n\n",
        settings.model,
        paths.db.display(),
        settings.rag_strategy,
        if settings.chatmem { "on" } else { "off" }
    );
    md += &summary(rows);
    for sc in rows {
        md += &format!("\n## Сценарий {}: {}\n\nОжидаемая цель: {}\n\nЦель в памяти после диалога: «{}» ({}/{})\n\n", sc.id, sc.title, sc.goal, sc.goal_text, sc.goal_covered, sc.goal_must);
        md += &format!("Память в конце:\n\n```json\n{}\n```\n\n", serde_json::to_string_pretty(&sc.mem).unwrap_or_default());
        md += "| # | вопрос | статус | источники | ожидание | память |\n|---|---|---|---|---|---|\n";
        for t in &sc.turns {
            let cov = if t.must == 0 { "—".into() } else { format!("{}/{}", t.covered, t.must) };
            let mem = if t.state_missing.is_empty() && t.state_checks > 0 {
                "✓".into()
            } else if t.state_checks == 0 {
                "—".into()
            } else {
                format!("нет: {}", t.state_missing.join(", "))
            };
            md += &format!("| {} | {} | {} (ждали {}) | {}/{} | {cov} | {mem} |\n", t.n, t.q, t.status, t.expect, t.sources_ok, t.sources);
        }
        md += "\n### Ответы\n";
        for t in &sc.turns {
            md += &format!("\n#### {}.{:02} {}\n\n", sc.id, t.n, t.q);
            if !t.context.is_empty() {
                md += "_контекст:_\n";
                for (i, c) in t.context.iter().enumerate() {
                    md += &format!("{}. {c}\n", i + 1);
                }
                md += "\n";
            }
            match &t.card {
                Some(k) => {
                    for l in k.to_text().lines() {
                        md += &format!("> {l}\n");
                    }
                    if !k.grounded() && k.status == Status::Answer {
                        md += &format!("\n- ✗ проверки: {}", k.verdict());
                    }
                }
                None => md += &format!("> ошибка: {}\n", t.error.as_deref().unwrap_or("")),
            }
            if let Some(e) = &t.mem_error {
                md += &format!("\n- память не обновилась: {e}\n");
            }
        }
    }
    md
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> TaskMem {
        TaskMem {
            goal: "спроектировать Raft-сервис конфигураций".into(),
            clarified: vec!["чтения должны быть строго актуальными".into()],
            constraints: vec!["ровно 5 узлов".into()],
            terms: vec![Term { term: "кворум".into(), meaning: "3 из 5".into() }],
        }
    }

    #[test]
    fn the_block_is_one_tag_delimited_piece_and_none_when_empty() {
        assert!(TaskMem::default().block().is_none());
        let b = mem().block().unwrap();
        assert!(b.starts_with("<task-state>\n") && b.ends_with("</task-state>\n\n"));
        assert_eq!(b.matches("<task-state>").count(), 1);
        assert!(b.contains("Goal of this dialogue: спроектировать Raft-сервис конфигураций"));
        assert!(b.contains("Fixed constraint: ровно 5 узлов"));
        assert!(b.contains("Agreed term: кворум = 3 из 5"));
    }

    #[test]
    fn the_retrieval_query_carries_the_goal_and_constraints() {
        let q = "а таймауты?";
        assert_eq!(TaskMem::default().retrieval_query(q), q);
        let got = mem().retrieval_query(q);
        assert!(got.starts_with("а таймауты?\n(контекст задачи: спроектировать Raft-сервис конфигураций"));
        assert!(got.contains("ограничения: ровно 5 узлов") && got.contains("термины: кворум"));
        // a state with only clarifications has nothing to add to the query
        let mut m = mem();
        m.goal.clear();
        m.constraints.clear();
        m.terms.clear();
        assert_eq!(m.retrieval_query(q), q);
    }

    #[test]
    fn memory_entries_number_the_state_and_the_user_replies_only() {
        let hist = vec![ChatMessage::user("меня  зовут Шурик"), ChatMessage::assistant("Не знаю: …"), ChatMessage::user("какой кворум?")];
        let e = memory_entries(&mem(), &hist);
        assert_eq!(e[0], ("цель".to_string(), "спроектировать Raft-сервис конфигураций".to_string()));
        assert!(e.iter().any(|(k, t)| k == "ограничение" && t == "ровно 5 узлов"));
        assert!(e.iter().any(|(k, t)| k == "термин" && t == "кворум = 3 из 5"));
        // ответы ассистента — не записи памяти; реплики пользователя — да, по порядку
        assert!(!e.iter().any(|(_, t)| t.starts_with("Не знаю")));
        assert_eq!(e[e.len() - 2], ("реплика пользователя №1".to_string(), "меня зовут Шурик".to_string()));
        assert_eq!(e.last().unwrap().0, "реплика пользователя №2");
        assert!(memory_entries(&TaskMem::default(), &[]).is_empty());
    }

    #[test]
    fn recall_prompt_carries_the_contract_and_the_numbered_entries() {
        let e = memory_entries(&mem(), &[ChatMessage::user("меня зовут Шурик")]);
        let p = recall_prompt("какой кворум?", &e);
        assert!(p.contains("ONLY the numbered memory entries"));
        assert!(p.contains(NOT_IN_MEMORY));
        assert!(p.contains("\"sources\""));
        assert!(p.contains("[M1] цель: спроектировать Raft-сервис конфигураций\n"));
        assert!(p.contains(&format!("[M{}] реплика пользователя №1: меня зовут Шурик\n", e.len())));
        assert!(p.ends_with("Question: какой кворум?"));
    }

    #[test]
    fn parse_recall_checks_every_memory_quote_against_its_entry() {
        let e = memory_entries(&mem(), &[ChatMessage::user("меня зовут Шурик")]);
        let last = e.len();
        let reply = format!(
            "```json\n{{\"answer\":\"Вас зовут Шурик, кворум — 3 из 5.\",\"sources\":[\
             {{\"n\":{last},\"quote\":\"зовут Шурик\"}},{{\"n\":\"M{last}\",\"quote\":\"dup\"}},\
             {{\"n\":1,\"quote\":\"кворум 3 из 5\"}},{{\"n\":99,\"quote\":\"нет такой записи\"}}]}}```"
        );
        let (answer, src) = parse_recall(&reply, &e);
        assert_eq!(answer, "Вас зовут Шурик, кворум — 3 из 5.");
        // M99 отброшен, дубликат M{last} тоже; цитата к цели — не из цели
        assert_eq!(src.len(), 2);
        assert!(src[0].n == last && src[0].quote_ok && src[0].kind == "реплика пользователя №1");
        assert!(src[1].n == 1 && !src[1].quote_ok);
        // не JSON — ответ без источников, не выдуманные ссылки
        let (a, s) = parse_recall("Вас зовут Шурик", &e);
        assert_eq!(a, "Вас зовут Шурик");
        assert!(s.is_empty());
    }

    #[test]
    fn recall_card_lists_memory_sources_and_never_corpus_ones() {
        let src = cite::MemSource { n: 2, kind: "термин".into(), text: "кворум = 3 из 5".into(), quote: "3 из 5".into(), quote_ok: true };
        let c = Card::recall("кворум — 3, как договорились", vec![src], &Usage::default(), 7);
        assert!(c.from_memory && c.sources.is_empty() && c.memory_backed());
        let t = c.to_text();
        assert!(t.contains("Источники (память диалога):\n  [M2] термин: кворум = 3 из 5 · «3 из 5» ✓"));
        assert!(t.contains("Из памяти задачи: ответ дан из памяти этого диалога"));
        assert!(!t.contains("Проверка:"));
        // без записи памяти — честное «НЕ подтверждён»
        let bare = Card::recall("кворум — 3", vec![], &Usage::default(), 7);
        assert!(!bare.memory_backed());
        assert!(bare.to_text().contains("НЕ подтверждён"));
    }

    #[test]
    fn parse_state_validates_caps_and_dedupes() {
        let text = "пролог ```json\n{\"goal\":\"  цель   диалога \",\"clarified\":[\"а\",\"А\",\"  \"],\"constraints\":[\"x\"] ,\"terms\":[{\"term\":\"t\",\"meaning\":\"m\"},{\"term\":\"T\",\"meaning\":\"m2\"},{\"term\":\"\",\"meaning\":\"drop\"}]}```";
        let m = parse_state(text).unwrap();
        assert_eq!(m.goal, "цель диалога");
        assert_eq!(m.clarified, vec!["а"]);
        assert_eq!(m.terms.len(), 1);
        assert!(parse_state("no json at all").is_none());
        assert!(parse_state("{\"goal\": 5}").is_none());
        let huge = format!("{{\"clarified\":[{}]}}", (0..50).map(|i| format!("\"item{i}\"")).collect::<Vec<_>>().join(","));
        assert_eq!(parse_state(&huge).unwrap().clarified.len(), MAX_CLARIFIED);
    }
}
