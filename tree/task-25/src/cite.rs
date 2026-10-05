//! Task 24: citations, sources and the "I don't know" mode.
//!
//! A RAG answer is no longer free text. The model must reply with one JSON
//! object — the answer, the sources it used (`source` + `section` +
//! `chunk_id`, exactly as in the excerpt headers) and verbatim quotes from
//! those excerpts — and every part of it is checked against the chunks that
//! were actually handed over, not taken on trust:
//!
//! * a source must name an excerpt that was in the context, and its
//!   `chunk_id` must be that excerpt's id;
//! * a quote must occur in the cited chunk character for character (up to
//!   case, whitespace, punctuation and pdftotext's line-break hyphens);
//! * every number of the answer must occur in the quotes (or the question);
//! * a separate LLM call at temperature 0 sees only the answer and the
//!   quotes and says whether the quotes carry the answer's meaning.
//!
//! A reply that is not JSON, has no valid source or no verbatim quote gets
//! one retry with the problems listed. A reply whose quotes do not carry
//! some of its claims (the judge says `partial`/`no`) gets one more: quote
//! what backs them or drop them. What is still wrong after that is shown in
//! red rather than hidden.
//!
//! The "I don't know" rule runs *before* the model: when the best chunk of
//! the context is below the relevance threshold (reranker score
//! `rag_idk_llm`, or the z-score `rag_idk_z` when the reranker did not run)
//! or nothing passed the filter, the assistant says it does not know and
//! asks to clarify, naming the nearest candidates — no generation, so
//! nothing to hallucinate. The model may also answer `"status":"unknown"`
//! itself when the excerpts turn out not to hold the answer.
//!
//! `ask --rag-cite-eval` checks it on ten questions (one per document) plus
//! the five without an answer and three vague ones → `rag/cite.md`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Instant;

use crate::agent::Agent;
use crate::api::{ChatMessage, Usage};
use crate::config::{Res, Settings};
use crate::ragqa::{self, Control, Hit, Retriever};
use crate::rerank::{self, Judge, Pipeline, Retrieval};

/// The reranker gives 9–10 to the best chunk of every answerable control
/// question and at most 4 to "near misses" (Sycamore, GW170817): 7 sits in
/// the gap — "contains a substantial part of the answer" on its scale.
pub const DEFAULT_IDK_LLM: u8 = 7;
/// Without the reranker only the z-score is left. It cannot separate near
/// misses from answers (task 23: GW170817 has z 3.03, real answers 2.6–3.0),
/// so this is a weak fallback, not the main gate.
pub const DEFAULT_IDK_Z: f32 = 3.0;
/// Quotes shorter than this (letters and digits only) prove nothing.
const MIN_QUOTE: usize = 12;

/// The questions `--rag-cite-eval` asks: one per document of the corpus.
pub const EVAL_ANSWER: [usize; 10] = [1, 3, 5, 7, 9, 11, 14, 15, 17, 20];
/// Control questions without an answer in the corpus.
pub const EVAL_UNKNOWN: [usize; 5] = [22, 23, 24, 25, 26];
/// Questions too vague to answer from any one document.
pub const EVAL_VAGUE: [&str; 3] = ["How fast is it?", "What was the final result?", "Which method works best?"];

// ---------------------------------------------------------------- thresholds

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Idk {
    pub llm: u8,
    pub z: f32,
}

impl Idk {
    pub fn from_settings(s: &Settings) -> Idk {
        Idk { llm: s.rag_idk_llm, z: s.rag_idk_z }
    }

    pub fn label(&self) -> String {
        format!("«не знаю» при llm < {} (без реранкера — z < {:.1})", self.llm, self.z)
    }
}

/// `llm 9` / `z 3.12` — the relevance the gate looks at.
fn relevance(h: &Hit) -> String {
    match h.llm {
        Some(s) => format!("llm {s}"),
        None => format!("z {:.2}", h.z),
    }
}

/// Why the context is too weak to answer from, or `None` — go ahead.
pub fn weak(got: &Retrieval, idk: Idk) -> Option<String> {
    let Some(best) = got.kept.first() else {
        return Some(match got.pool.iter().max_by(|a, b| rank_key(a).total_cmp(&rank_key(b))) {
            Some(b) => format!("ни один фрагмент не прошёл фильтр (лучший кандидат — {})", relevance(b)),
            None => "поиск ничего не нашёл".into(),
        });
    };
    if got.kept.iter().any(|h| h.llm.is_some()) {
        let top = got.kept.iter().filter_map(|h| h.llm).max().unwrap_or(0);
        (top < idk.llm).then(|| format!("лучший фрагмент — llm {top} < порога {}", idk.llm))
    } else {
        let top = got.kept.iter().map(|h| h.z).fold(best.z, f32::max);
        (top < idk.z).then(|| format!("лучший фрагмент — z {top:.2} < порога {:.1}", idk.z))
    }
}

fn rank_key(h: &Hit) -> f32 {
    h.llm.map(|s| 100.0 + s as f32).unwrap_or(h.z)
}

/// "Not sure what you mean" hints: the nearest candidates, best first.
fn nearest(got: &Retrieval, n: usize) -> Vec<String> {
    let mut pool: Vec<&Hit> = got.pool.iter().collect();
    pool.sort_by(|a, b| rank_key(b).total_cmp(&rank_key(a)));
    let mut out: Vec<String> = Vec::new();
    for h in pool {
        let line = format!("{} — {} ({})", h.chunk.file, h.cite(), relevance(h));
        if out.len() < n && !out.iter().any(|o| o.starts_with(&format!("{} — {}", h.chunk.file, h.cite()))) {
            out.push(line);
        }
    }
    out
}

// ---------------------------------------------------------------- prompt

/// What goes to the model instead of the bare question: the contract, the
/// excerpts with `source` / `section` / `chunk_id`, then the question.
/// `mem` (task 25) adds one `<task-state>` block between the contract and
/// the context — tag-delimited, so the rest of the prompt stays
/// byte-identical when the dialogue's state moves.
pub fn prompt(question: &str, hits: &[Hit], mem: Option<&crate::chatmem::TaskMem>) -> String {
    let mut s = String::from(
        "Answer the question using only the document excerpts below. Reply with ONE JSON object and nothing else:\n\
         {\"status\":\"answer\",\"answer\":\"… [1] …\",\
         \"sources\":[{\"n\":1,\"source\":\"<source>\",\"section\":\"<section>\",\"chunk_id\":\"<chunk_id>\"}],\
         \"quotes\":[{\"n\":1,\"text\":\"<fragment copied verbatim from excerpt 1>\"}]}\n\
         Rules:\n\
         - answer: concise, in the language of the question; mark the excerpts you rely on as [1], [2], …\n\
         - sources: every excerpt you used, with source, section and chunk_id copied exactly from its header.\n\
         - quotes: 1–3 fragments (one sentence or a clause, at most ~300 characters each) copied character for \
         character from excerpt n — no paraphrase, no translation, no ellipsis inside; n is the number of the excerpt \
         the fragment comes from, not the position of the quote. Every fact and number of the answer must be visible in \
         the quotes.\n\
         - If the excerpts do not contain the answer, do not answer from your own knowledge; reply \
         {\"status\":\"unknown\",\"answer\":\"\",\"sources\":[],\"quotes\":[],\"clarify\":\"<one question asking the \
         user to clarify, in the language of the question>\"}\n\n",
    );
    if let Some(block) = mem.and_then(crate::chatmem::TaskMem::block) {
        s += &block;
    }
    s += "<context>\n";
    for (i, h) in hits.iter().enumerate() {
        let c = &h.chunk;
        s += &format!(
            "[{}] source={} | section={} | pages={} | chunk_id={}\n{}\n\n",
            i + 1,
            c.file,
            c.section,
            pages(h),
            c.chunk_id,
            c.text.trim()
        );
    }
    if hits.is_empty() {
        s += "(the search found no relevant excerpts)\n";
    }
    s += "</context>\n\nQuestion: ";
    s += question.trim();
    s
}

fn pages(h: &Hit) -> String {
    let c = &h.chunk;
    if c.page_start == c.page_end { format!("стр. {}", c.page_start) } else { format!("стр. {}–{}", c.page_start, c.page_end) }
}

// ---------------------------------------------------------------- parsing

/// The reply as the model sent it.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Raw {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub answer: String,
    #[serde(default)]
    pub sources: Vec<RawSource>,
    #[serde(default)]
    pub quotes: Vec<RawQuote>,
    #[serde(default)]
    pub clarify: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct RawSource {
    #[serde(default, deserialize_with = "loose_n")]
    pub n: usize,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub section: String,
    #[serde(default)]
    pub chunk_id: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct RawQuote {
    #[serde(default, deserialize_with = "loose_n")]
    pub n: usize,
    #[serde(default)]
    pub text: String,
}

/// `1`, `"1"` or `"[1]"`.
fn loose_n<'de, D: serde::Deserializer<'de>>(d: D) -> Result<usize, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(match v {
        Value::Number(n) => n.as_u64().unwrap_or(0) as usize,
        Value::String(s) => s.trim_matches(|c: char| !c.is_ascii_digit()).parse().unwrap_or(0),
        _ => 0,
    })
}

/// The JSON object of a reply, maybe fenced or with prose around it.
pub fn parse(text: &str) -> Option<Raw> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    serde_json::from_str(text.get(start..=end)?).ok()
}

// ---------------------------------------------------------------- checking

/// Letters and digits only, lowercased: what survives pdftotext's line
/// breaks, hyphenation and the model's choice of quote marks and dashes.
pub(crate) fn compact(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Found {
    /// In the excerpt it is attributed to.
    Verbatim,
    /// Verbatim, but in another excerpt of the context (1-based).
    Elsewhere(usize),
    /// Too short to prove anything.
    Short,
    /// Not in any excerpt — paraphrased or made up.
    Missing,
}

impl Found {
    pub fn ok(self) -> bool {
        matches!(self, Found::Verbatim | Found::Elsewhere(_))
    }
}

/// Where a quote occurs among `hits`, attributed to excerpt `n`.
pub fn locate(quote: &str, n: usize, hits: &[Hit]) -> Found {
    let q = compact(quote);
    if q.chars().count() < MIN_QUOTE {
        return Found::Short;
    }
    let inside = |h: &Hit| compact(&h.chunk.text).contains(&q);
    if hits.get(n.wrapping_sub(1)).is_some_and(inside) {
        return Found::Verbatim;
    }
    match hits.iter().position(inside) {
        Some(i) => Found::Elsewhere(i + 1),
        None => Found::Missing,
    }
}

/// Numbers of the answer (`5,000` → `5000`, `3.0` → `30`), citation marks
/// `[n]` aside.
pub fn numbers(text: &str) -> Vec<String> {
    let mut clean = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '[' {
            let mut inner = String::new();
            while let Some(&d) = chars.peek() {
                if d.is_ascii_digit() || d == ',' || d == ' ' || d == '–' || d == '-' {
                    inner.push(d);
                    chars.next();
                } else {
                    break;
                }
            }
            if chars.peek() == Some(&']') && inner.chars().any(|d| d.is_ascii_digit()) {
                chars.next();
                clean.push(' ');
                continue;
            }
            clean.push('[');
            clean.push_str(&inner);
            continue;
        }
        clean.push(c);
    }
    let mut out: Vec<String> = Vec::new();
    let b: Vec<char> = clean.chars().collect();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() && (i == 0 || !b[i - 1].is_alphanumeric()) {
            let mut j = i;
            while j < b.len() && (b[j].is_ascii_digit() || ((b[j] == ',' || b[j] == '.') && b.get(j + 1).is_some_and(|c| c.is_ascii_digit()))) {
                j += 1;
            }
            // `GPT-4`, `LLaMA-2` are names, not quantities (`x86` never starts here)
            if !(i >= 2 && b[i - 1] == '-' && b[i - 2].is_alphabetic()) {
                let n: String = b[i..j].iter().filter(|c| c.is_ascii_digit()).collect();
                if !out.contains(&n) {
                    out.push(n);
                }
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    out
}

#[derive(Clone, Debug, Serialize)]
pub struct Source {
    pub n: usize,
    pub file: String,
    pub section: String,
    pub pages: String,
    pub chunk_id: String,
    /// The model copied the right chunk_id for excerpt n.
    pub id_ok: bool,
    pub relevance: String,
}

/// Задача 25: источник ответа «из памяти задачи» — запись памяти диалога
/// (пункт состояния задачи или реплика пользователя), на которую сослалась
/// модель, и дословная цитата из неё.
#[derive(Clone, Debug, Serialize)]
pub struct MemSource {
    /// Номер записи в списке, который видела модель (`[M1]`…).
    pub n: usize,
    /// «цель», «уточнено», «ограничение», «термин», «реплика пользователя №k».
    pub kind: String,
    /// Запись памяти целиком.
    pub text: String,
    /// Что модель привела в подтверждение.
    pub quote: String,
    /// Цитата действительно есть в этой записи (буквы и цифры, без регистра).
    pub quote_ok: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Quote {
    pub n: usize,
    pub text: String,
    pub found: Found,
}

/// What the model was told to say and what of it held up.
#[derive(Clone, Debug, Serialize)]
pub struct Check {
    pub json_ok: bool,
    /// Valid sources (point at an excerpt of the context) / listed.
    pub sources_ok: usize,
    pub sources: usize,
    /// Verbatim quotes / given.
    pub quotes_ok: usize,
    pub quotes: usize,
    /// Numbers of the answer that occur in no quote and not in the question.
    pub unbacked: Vec<String>,
    pub numbers: usize,
}

impl Check {
    pub fn has_sources(&self) -> bool {
        self.sources_ok > 0
    }

    pub fn has_quotes(&self) -> bool {
        self.quotes_ok > 0
    }

    /// Everything the contract asks for is there and verifiable.
    pub fn grounded(&self) -> bool {
        self.json_ok && self.has_sources() && self.has_quotes() && self.sources_ok == self.sources && self.quotes_ok == self.quotes && self.unbacked.is_empty()
    }
}

/// The independent "does the meaning of the answer follow from the quotes".
#[derive(Clone, Debug, Serialize)]
pub struct Support {
    /// `yes` / `partial` / `no`.
    pub verdict: String,
    pub unsupported: Vec<String>,
}

impl Support {
    pub fn yes(&self) -> bool {
        self.verdict == "yes"
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Answer,
    Unknown,
}

/// Who said "I don't know".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum By {
    /// The relevance gate, before any generation.
    Gate,
    /// The model, from the excerpts it was given.
    Model,
}

/// One grounded reply, ready to show: the chat's card, the CLI's printout,
/// a row of the eval.
#[derive(Clone, Debug, Serialize)]
pub struct Card {
    pub status: Status,
    pub by: Option<By>,
    pub answer: String,
    pub sources: Vec<Source>,
    pub quotes: Vec<Quote>,
    pub clarify: Option<String>,
    /// Gate: why the context is too weak; nearest candidates.
    pub why: Option<String>,
    pub nearest: Vec<String>,
    pub check: Option<Check>,
    pub support: Option<Support>,
    /// Contract violations of the first attempt that caused the retry.
    pub retried: Vec<String>,
    pub attempts: usize,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub ms: u128,
    pub finish_reason: Option<String>,
    /// The last reply as the model sent it.
    pub raw: String,
    /// Задача 25: ответ дан из памяти задачи (цели и договорённостей
    /// диалога), а не из фрагментов корпуса — у него по определению нет
    /// источников, и «Проверка: …» к нему не относится. Ставит только
    /// `chatmem::Chat::recall`, когда вопрос — о договорённостях.
    #[serde(default)]
    pub from_memory: bool,
    /// Источники ответа из памяти: записи памяти диалога с проверенными
    /// цитатами. Пусто у обычных ответов.
    #[serde(default)]
    pub mem_sources: Vec<MemSource>,
    /// Usage of the first attempt alone — the one whose prompt the chat
    /// measured, for the footer's chars-per-token calibration.
    #[serde(skip)]
    pub first_usage: Usage,
}

impl Card {
    /// The gate's "I don't know": no model call at all.
    pub fn unknown(question: &str, got: &Retrieval, why: String) -> Card {
        let nearest = nearest(got, 3);
        Card {
            status: Status::Unknown,
            by: Some(By::Gate),
            answer: String::new(),
            sources: vec![],
            quotes: vec![],
            clarify: Some(clarify_default(question, &nearest, got.rewritten.is_none())),
            why: Some(why),
            nearest,
            check: None,
            support: None,
            retried: vec![],
            attempts: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            ms: 0,
            finish_reason: None,
            raw: String::new(),
            from_memory: false,
            mem_sources: vec![],
            first_usage: Usage::default(),
        }
    }

    /// Задача 25: карточка ответа «из памяти задачи» (chatmem.rs). Вопрос
    /// был о договорённостях диалога: корпуса под ним нет по определению,
    /// поэтому источники — записи памяти диалога, а не фрагменты.
    pub fn recall(answer: &str, mem_sources: Vec<MemSource>, usage: &Usage, ms: u128) -> Card {
        Card { status: Status::Answer, by: None, answer: answer.trim().to_string(), sources: vec![], quotes: vec![], clarify: None, why: None, nearest: vec![], check: None, support: None, retried: vec![], attempts: 0, prompt_tokens: usage.prompt_tokens, completion_tokens: usage.completion_tokens, ms, finish_reason: None, raw: String::new(), from_memory: true, mem_sources, first_usage: *usage }
    }

    /// Ответ из памяти, и каждая его ссылка на память подтверждена цитатой.
    pub fn memory_backed(&self) -> bool {
        self.from_memory && !self.mem_sources.is_empty() && self.mem_sources.iter().all(|m| m.quote_ok)
    }

    /// Answered, and every part of the answer checks out.
    pub fn grounded(&self) -> bool {
        self.status == Status::Answer && self.check.as_ref().is_some_and(Check::grounded) && self.support.as_ref().is_none_or(Support::yes)
    }

    /// One line verdict: `источники 2/2 · цитаты 2/2 дословно · числа 3/3 · смысл: да`.
    pub fn verdict(&self) -> String {
        if self.status == Status::Unknown {
            return match self.by {
                Some(By::Gate) => "не знаю — порог релевантности, модель не вызывалась".into(),
                _ => "не знаю — модель не нашла ответа во фрагментах".into(),
            };
        }
        let Some(c) = &self.check else { return String::new() };
        let mark = |ok: bool| if ok { '✓' } else { '✗' };
        let mut parts = vec![];
        if !c.json_ok {
            parts.push("✗ ответ не в формате JSON".to_string());
        }
        parts.push(format!("{} источники {}/{}", mark(c.has_sources() && c.sources_ok == c.sources), c.sources_ok, c.sources));
        let elsewhere = self.quotes.iter().filter(|q| matches!(q.found, Found::Elsewhere(_))).count();
        parts.push(format!(
            "{} цитаты {}/{} дословно{}",
            mark(c.has_quotes() && c.quotes_ok == c.quotes),
            c.quotes_ok,
            c.quotes,
            if elsewhere > 0 { format!(" ({elsewhere} — из другого фрагмента)") } else { String::new() }
        ));
        if c.numbers > 0 {
            parts.push(format!("{} числа {}/{} в цитатах", mark(c.unbacked.is_empty()), c.numbers - c.unbacked.len(), c.numbers));
        }
        if let Some(s) = &self.support {
            let word = match s.verdict.as_str() {
                "yes" => "да",
                "partial" => "частично",
                "no" => "нет",
                other => other,
            };
            parts.push(format!("{} смысл по цитатам: {word}", mark(s.yes())));
        }
        parts.join(" · ")
    }

    /// Plain text for the session history and the CLI.
    pub fn to_text(&self) -> String {
        let mut s = String::new();
        if self.status == Status::Unknown {
            s += "Не знаю: в найденных фрагментах нет ответа на этот вопрос.";
            if let Some(w) = &self.why {
                s += &format!(" ({w})");
            }
            if let Some(c) = &self.clarify {
                s += &format!("\nУточните, пожалуйста: {c}");
            }
            for n in &self.nearest {
                s += &format!("\n  ближе всего: {n}");
            }
            return s;
        }
        s += self.answer.trim();
        if !self.sources.is_empty() {
            s += "\n\nИсточники:";
            for src in &self.sources {
                s += &format!("\n  [{}] {} — {}, {} · {}", src.n, src.file, src.section, src.pages, src.chunk_id);
            }
        }
        if !self.quotes.is_empty() {
            s += "\n\nЦитаты:";
            for q in &self.quotes {
                s += &format!("\n  [{}] «{}»", q.n, q.text.trim());
            }
        }
        if self.from_memory {
            // Из памяти задачи: источники — записи памяти диалога, а не
            // фрагменты корпуса; подпись честно говорит, откуда ответ.
            if !self.mem_sources.is_empty() {
                s += "\n\nИсточники (память диалога):";
                for m in &self.mem_sources {
                    s += &format!("\n  [M{}] {}: {} · «{}» {}", m.n, m.kind, m.text, m.quote.trim(), if m.quote_ok { '✓' } else { '✗' });
                }
            }
            s += if self.memory_backed() {
                "\n\nИз памяти задачи: ответ дан из памяти этого диалога, а не из фрагментов корпуса."
            } else {
                "\n\nИз памяти задачи: ответ НЕ подтверждён — модель не привела запись памяти с дословной цитатой."
            };
        } else {
            let v = self.verdict();
            if !v.is_empty() {
                s += &format!("\n\nПроверка: {v}");
            }
        }
        s
    }
}

/// The gate's clarifying question. The corpus and its embeddings are
/// English: a question in another language without the rewrite rarely
/// finds anything, so say that too.
fn clarify_default(question: &str, nearest: &[String], not_rewritten: bool) -> String {
    let mut s = format!(
        "о каком документе или системе вопрос «{}», и что именно нужно узнать (термин, число, раздел)?",
        question.trim()
    );
    if !nearest.is_empty() {
        s += " Например, речь о чём-то из этого?";
    }
    if not_rewritten && question.chars().any(|c| matches!(c, 'а'..='я' | 'А'..='Я' | 'ё' | 'Ё')) {
        s += " Документы на английском: спросите по-английски или включите /rag rewrite on — он переведёт запрос.";
    }
    s
}

/// The model's reply checked against the excerpts it was given.
pub fn verify(text: &str, question: &str, hits: &[Hit]) -> Card {
    let raw = parse(text);
    let json_ok = raw.is_some();
    let r = raw.unwrap_or_else(|| Raw { status: "answer".into(), answer: text.trim().to_string(), ..Raw::default() });
    let unknown = r.status.trim().eq_ignore_ascii_case("unknown");
    let mut sources: Vec<Source> = Vec::new();
    let mut bad_sources = 0;
    for s in &r.sources {
        // the chunk_id names the excerpt more surely than `n` (models number
        // their sources by position too); `n` only when the id matches none
        let n = match hits.iter().position(|h| h.chunk.chunk_id == s.chunk_id.trim()) {
            Some(i) => i + 1,
            None if (1..=hits.len()).contains(&s.n) => s.n,
            None => 0,
        };
        let Some(h) = hits.get(n.wrapping_sub(1)) else {
            bad_sources += 1;
            continue;
        };
        if sources.iter().any(|x| x.n == n) {
            continue;
        }
        sources.push(Source {
            n,
            file: h.chunk.file.clone(),
            section: h.chunk.section.clone(),
            pages: pages(h),
            chunk_id: h.chunk.chunk_id.clone(),
            id_ok: s.chunk_id.trim() == h.chunk.chunk_id
                && (s.source.trim().is_empty() || s.source.trim() == h.chunk.file)
                // the model may append the pages to the section — the name itself must be there
                && (s.section.trim().is_empty() || s.section.contains(h.chunk.section.as_str())),
            relevance: relevance(h),
        });
    }
    let quotes: Vec<Quote> = r
        .quotes
        .iter()
        .filter(|q| !q.text.trim().is_empty())
        .map(|q| Quote { n: q.n, text: q.text.trim().to_string(), found: locate(&q.text, q.n, hits) })
        .collect();
    let quoted: String = quotes.iter().filter(|q| q.found.ok()).map(|q| compact(&q.text)).collect::<Vec<_>>().join(" ");
    let asked = compact(question);
    let nums = numbers(&r.answer);
    let unbacked: Vec<String> = nums.iter().filter(|n| !quoted.contains(n.as_str()) && !asked.contains(n.as_str())).cloned().collect();
    let ok_sources = sources.iter().filter(|s| s.id_ok).count();
    let check = Check {
        json_ok,
        sources_ok: ok_sources,
        sources: sources.len() + bad_sources,
        quotes_ok: quotes.iter().filter(|q| q.found.ok()).count(),
        quotes: quotes.len(),
        unbacked,
        numbers: nums.len(),
    };
    Card {
        status: if unknown { Status::Unknown } else { Status::Answer },
        by: unknown.then_some(By::Model),
        answer: r.answer.trim().to_string(),
        sources,
        quotes,
        clarify: r.clarify.filter(|c| !c.trim().is_empty()).or_else(|| unknown.then(|| clarify_default(question, &[], false))),
        why: None,
        nearest: vec![],
        check: (!unknown).then_some(check),
        support: None,
        retried: vec![],
        attempts: 1,
        prompt_tokens: 0,
        completion_tokens: 0,
        ms: 0,
        finish_reason: None,
        raw: text.to_string(),
        from_memory: false,
        mem_sources: vec![],
        first_usage: Usage::default(),
    }
}

/// What to tell the model on the retry; empty — the reply is fine.
pub fn problems(card: &Card, hits: &[Hit]) -> Vec<String> {
    let Some(c) = &card.check else { return vec![] };
    let mut out = Vec::new();
    if !c.json_ok {
        out.push("the reply was not a JSON object".to_string());
    }
    if card.answer.is_empty() {
        out.push("\"answer\" is empty".into());
    }
    if !c.has_sources() {
        out.push(format!("\"sources\" has no valid entry: n must be 1..{} and chunk_id copied from that excerpt's header", hits.len()));
    } else if c.sources_ok < c.sources {
        out.push("some sources point at no excerpt or carry a wrong chunk_id".into());
    }
    for q in &card.quotes {
        match q.found {
            Found::Missing => out.push(format!("quote [{}] «{}» is not in any excerpt — copy it character for character", q.n, q.text)),
            Found::Short => out.push(format!("quote [{}] «{}» is too short to check — quote a whole clause", q.n, q.text)),
            _ => {}
        }
    }
    if c.quotes == 0 {
        out.push("\"quotes\" is empty: give 1–3 verbatim fragments".into());
    }
    if !c.unbacked.is_empty() {
        out.push(format!("numbers {} of the answer are in none of the quotes — quote where they come from or drop them", c.unbacked.join(", ")));
    }
    out
}

// ---------------------------------------------------------------- the turn

/// The "do the quotes carry the answer" judge: the user's model at
/// temperature 0, no system extras.
pub struct Checker {
    agent: Agent,
}

impl Checker {
    pub fn new(settings: &Settings) -> Res<Checker> {
        let mut agent = ragqa::eval_agent(settings)?;
        agent.settings_mut().temperature = Some(0.0);
        agent.settings_mut().system_prompt = "You are a strict fact-checker. Follow the output format exactly.".into();
        Ok(Checker { agent })
    }

    pub fn support(&self, question: &str, card: &Card) -> Res<(Support, Usage)> {
        let mut p = format!(
            "Decide whether the meaning of the answer below is supported by the quotes alone (ignore your own knowledge).\n\
             yes = every factual claim of the answer is stated in the quotes or follows directly from them;\n\
             partial = the main claim is supported, but some details are not in the quotes;\n\
             no = the main claim is not in the quotes or contradicts them.\n\
             Reply with JSON only: {{\"verdict\":\"yes|partial|no\",\"unsupported\":[\"claim not backed by the quotes\", …]}}\n\n\
             Question: {}\n\nAnswer: {}\n\nQuotes:\n",
            question.trim(),
            card.answer
        );
        for q in &card.quotes {
            p += &format!("[{}] \"{}\"\n", q.n, q.text);
        }
        let o = self.agent.complete_outcome(&[ChatMessage::user(p)])?;
        let text = o.text();
        let v: Value = text
            .find('{')
            .zip(text.rfind('}'))
            .and_then(|(a, b)| serde_json::from_str(text.get(a..=b)?).ok())
            .ok_or_else(|| format!("судья вернул не JSON: {}", text.chars().take(160).collect::<String>()))?;
        let verdict = v["verdict"].as_str().unwrap_or("").trim().to_lowercase();
        if !matches!(verdict.as_str(), "yes" | "partial" | "no") {
            return Err(format!("судья: неизвестный вердикт {verdict:?}"));
        }
        let unsupported = v["unsupported"].as_array().into_iter().flatten().filter_map(|x| x.as_str()).map(str::to_string).collect();
        Ok((Support { verdict, unsupported }, o.usage))
    }
}

/// One grounded turn. `history` ends with the user's question as typed; its
/// last message goes out as [`prompt`]. With a weak context the gate's
/// card comes back without calling the model.
pub fn answer(agent: &Agent, checker: Option<&Checker>, history: &[ChatMessage], question: &str, got: &Retrieval, idk: Idk, mem: Option<&crate::chatmem::TaskMem>) -> Res<Card> {
    if let Some(why) = weak(got, idk) {
        return Ok(Card::unknown(question, got, why));
    }
    let t = Instant::now();
    let hits = &got.kept;
    let mut wire = history.to_vec();
    match wire.last_mut() {
        Some(last) => last.content = prompt(question, hits, mem),
        None => wire.push(ChatMessage::user(prompt(question, hits, mem))),
    }
    let (mut prompt_tokens, mut completion_tokens) = (0, 0);
    let mut card: Card;
    let mut retried: Vec<String> = Vec::new();
    let (mut attempt, mut format_retry, mut judge_retry) = (0, false, false);
    let mut first = Usage::default();
    // the reply before the judge's retry, kept in case the retry breaks the format
    let mut before_judge: Option<Card> = None;
    loop {
        attempt += 1;
        let o = agent.complete_outcome(&wire)?;
        if attempt == 1 {
            first = o.usage;
        }
        prompt_tokens += o.usage.prompt_tokens;
        completion_tokens += o.usage.completion_tokens;
        card = verify(o.text(), question, hits);
        card.finish_reason = o.finish_reason.clone();
        let issues = problems(&card, hits);
        if !issues.is_empty() {
            if let Some(prev) = before_judge.take() {
                card = prev;
                retried.push("после повтора судьи формат сломался — оставлен предыдущий ответ".into());
                break;
            }
            if !format_retry {
                format_retry = true;
                wire.push(ChatMessage::assistant(o.text()));
                wire.push(ChatMessage::user(format!(
                    "Your reply breaks the required format:\n- {}\nReply again with the JSON object only, following every rule.",
                    issues.join("\n- ")
                )));
                retried.extend(issues);
                continue;
            }
        }
        if card.status != Status::Answer || card.quotes.is_empty() {
            break;
        }
        let Some(ch) = checker else { break };
        match ch.support(question, &card) {
            Ok((s, u)) => {
                prompt_tokens += u.prompt_tokens;
                completion_tokens += u.completion_tokens;
                card.support = Some(s);
            }
            Err(e) => {
                card.support = Some(Support { verdict: "?".into(), unsupported: vec![e] });
                break;
            }
        }
        let claims = card.support.as_ref().filter(|s| !s.yes()).map(|s| s.unsupported.clone()).unwrap_or_default();
        if claims.is_empty() || judge_retry || !issues.is_empty() {
            break;
        }
        // The quotes are verbatim but the answer says more than they do:
        // quote what backs the rest, or drop it.
        judge_retry = true;
        wire.push(ChatMessage::assistant(o.text()));
        wire.push(ChatMessage::user(format!(
            "A fact-checker compared your answer with your quotes only. These claims are in none of the quotes:\n- {}\n\
             For each claim either add a verbatim quote from the excerpts that states it, or remove the claim from the \
             answer. Reply again with the JSON object only, following every rule.",
            claims.join("\n- ")
        )));
        retried.push(format!("судья: не в цитатах — {}", claims.join("; ")));
        before_judge = Some(card.clone());
    }
    card.attempts = attempt;
    card.retried = retried;
    card.first_usage = first;
    card.prompt_tokens = prompt_tokens;
    card.completion_tokens = completion_tokens;
    card.ms = t.elapsed().as_millis();
    Ok(card)
}

/// `ask --rag "q"`: the whole grounded path, printed.
pub fn ask_once(question: &str, settings: &Settings) -> Res<()> {
    let p = ragqa::prepare(question, settings)?;
    eprintln!("{}", p.note);
    let agent = ragqa::eval_agent(settings)?;
    let checker = Checker::new(settings)?;
    let card = answer(&agent, Some(&checker), &[ChatMessage::user(question)], question, &p.retrieval, Idk::from_settings(settings), None)?;
    println!("{}", card.to_text());
    if !card.retried.is_empty() {
        eprintln!("· повторы: {}", card.retried.join("; "));
    }
    if card.status == Status::Answer && !card.grounded() {
        eprintln!("· ответ модели как есть: {}", card.raw);
    }
    eprintln!(
        "· попыток {} · {} prompt + {} completion tok · {:.1} с",
        card.attempts,
        card.prompt_tokens,
        card.completion_tokens,
        card.ms as f64 / 1000.0
    );
    Ok(())
}

// ---------------------------------------------------------------- eval

#[derive(Debug, Serialize)]
pub struct EvalRow {
    pub id: String,
    pub kind: &'static str,
    pub q: String,
    pub expect: String,
    pub must: Vec<String>,
    /// Groups of `must` in the answer text.
    pub covered: usize,
    /// Unanswerable: `wrong` groups the answer contains anyway.
    pub made_up: Vec<String>,
    pub pipeline: String,
    pub context: Vec<String>,
    pub card: Option<Card>,
    pub error: Option<String>,
    pub stage_tokens: u64,
}

/// `ask --rag-cite-eval`: ten answerable questions (one per document), the
/// five without an answer, three vague ones → `rag/cite.md`, `rag/cite.json`.
pub fn eval(settings: &Settings, paths: &crate::rag::Config, only: &[usize]) -> Res<()> {
    let controls = ragqa::load_controls(&paths.dir)?;
    let r = Retriever::open(&paths.db, &settings.rag_strategy, &paths.url)?;
    let p = Pipeline::from_settings(settings);
    let idk = Idk::from_settings(settings);
    let judge = if p.needs_llm() { Some(Judge::new(settings, &r)?) } else { None };
    let agent = ragqa::eval_agent(settings)?;
    let checker = Checker::new(settings)?;
    let mut set: Vec<(&'static str, Control)> = Vec::new();
    let pick = |id: usize| controls.iter().find(|c| c.id == id).cloned().ok_or_else(|| format!("control.json: нет вопроса {id}"));
    for id in EVAL_ANSWER {
        set.push(("answer", pick(id)?));
    }
    for id in EVAL_UNKNOWN {
        set.push(("unknown", pick(id)?));
    }
    for (i, q) in EVAL_VAGUE.iter().enumerate() {
        set.push((
            "vague",
            Control {
                id: 100 + i + 1,
                q: q.to_string(),
                expect: "не ясно, о каком документе речь: «не знаю» и просьба уточнить".into(),
                must: vec![],
                sources: vec![],
                at: vec![],
                wrong: vec![],
            },
        ));
    }
    if !only.is_empty() {
        set.retain(|(_, c)| only.contains(&c.id));
    }
    println!(
        "модель {} · индекс {} ({}, {} чанков) · {} · {} · вопросов {}",
        settings.model,
        r.db.display(),
        r.strategy,
        r.len(),
        p.label(),
        idk.label(),
        set.len()
    );
    let mut rows = Vec::new();
    for (kind, c) in &set {
        let label = if *kind == "vague" { format!("v{}", c.id - 100) } else { c.id.to_string() };
        println!("\n[{label}] {}", c.q);
        let got = rerank::run(&r, &p, judge.as_ref(), &c.q).map_err(|e| format!("[{label}] {e}"))?;
        let (card, error) = match answer(&agent, Some(&checker), &[ChatMessage::user(&c.q)], &c.q, &got, idk, None) {
            Ok(card) => (Some(card), None),
            Err(e) => (None, Some(e)),
        };
        let text = card.as_ref().map(|k| k.answer.clone()).unwrap_or_default();
        let row = EvalRow {
            id: label,
            kind,
            q: c.q.clone(),
            expect: c.expect.clone(),
            must: c.must.clone(),
            covered: ragqa::coverage(&text, &c.must).0,
            made_up: c.wrong.iter().filter(|g| ragqa::covers(&text, g)).cloned().collect(),
            pipeline: p.label(),
            context: got.kept.iter().map(ragqa::hit_line).collect(),
            card,
            error,
            stage_tokens: got.prompt_tokens + got.completion_tokens,
        };
        println!("    {}", progress(&row));
        rows.push(row);
    }
    let dir = paths.db.parent().map(Path::to_path_buf).unwrap_or_default();
    let (md_path, json_path) = (dir.join("cite.md"), dir.join("cite.json"));
    let md = report(settings, &r, &p, idk, &rows);
    std::fs::write(&md_path, &md).map_err(|e| format!("{}: {e}", md_path.display()))?;
    let raw = json!({"model": settings.model, "pipeline": p, "idk": idk, "rows": rows});
    std::fs::write(&json_path, serde_json::to_string_pretty(&raw).unwrap_or_default()).map_err(|e| format!("{}: {e}", json_path.display()))?;
    println!("\n{}", summary(&rows));
    println!("отчёт: {} (+ {})", md_path.display(), json_path.display());
    Ok(())
}

fn progress(row: &EvalRow) -> String {
    match (&row.card, &row.error) {
        (_, Some(e)) => format!("ОШИБКА: {e}"),
        (Some(k), _) => {
            let cov = if row.must.is_empty() { String::new() } else { format!(" · ожидание {}/{}", row.covered, row.must.len()) };
            let retry = if k.attempts > 1 { " · был повтор" } else { "" };
            format!("{}{cov}{retry} · {} tok · {:.1} с", k.verdict(), k.prompt_tokens + k.completion_tokens, k.ms as f64 / 1000.0)
        }
        _ => String::new(),
    }
}

pub fn summary(rows: &[EvalRow]) -> String {
    let mut md = String::from("| вопросы | ответил | источники есть | цитаты есть | все цитаты дословно | числа ответа в цитатах | смысл = цитаты (судья: да) | всё сразу | «не знаю» | из них порогом | выдумал ответ |\n|---|---|---|---|---|---|---|---|---|---|---|\n");
    for kind in ["answer", "unknown", "vague"] {
        let set: Vec<&EvalRow> = rows.iter().filter(|r| r.kind == kind).collect();
        if set.is_empty() {
            continue;
        }
        let cards: Vec<&Card> = set.iter().filter_map(|r| r.card.as_ref()).collect();
        let answered: Vec<&Card> = cards.iter().copied().filter(|k| k.status == Status::Answer).collect();
        let count = |f: &dyn Fn(&Card) -> bool| answered.iter().filter(|k| f(k)).count();
        let n = answered.len();
        let idk = cards.iter().filter(|k| k.status == Status::Unknown).count();
        let gate = cards.iter().filter(|k| k.by == Some(By::Gate)).count();
        let made_up = set.iter().filter(|r| !r.made_up.is_empty()).count();
        let name = match kind {
            "answer" => "с ответом в корпусе",
            "unknown" => "без ответа в корпусе",
            _ => "расплывчатые",
        };
        md += &format!(
            "| {name} ({}) | {n} | {}/{n} | {}/{n} | {}/{n} | {}/{n} | {}/{n} | {}/{n} | {idk} | {gate} | {} |\n",
            set.len(),
            count(&|k| k.check.as_ref().is_some_and(Check::has_sources)),
            count(&|k| k.check.as_ref().is_some_and(Check::has_quotes)),
            count(&|k| k.check.as_ref().is_some_and(|c| c.quotes > 0 && c.quotes_ok == c.quotes)),
            count(&|k| k.check.as_ref().is_some_and(|c| c.unbacked.is_empty())),
            count(&|k| k.support.as_ref().is_some_and(Support::yes)),
            count(&|k| k.grounded()),
            if kind == "answer" { "—".to_string() } else { made_up.to_string() },
        );
    }
    md
}

fn report(settings: &Settings, r: &Retriever, p: &Pipeline, idk: Idk, rows: &[EvalRow]) -> String {
    let mut md = format!(
        "# RAG: источники, цитаты и «не знаю» (задача 24)\n\n\
         Модель `{}`, индекс `{}` (стратегия `{}`, {} чанков), второй этап: {}, {}.\n\n\
         Каждый ответ — JSON с `answer`, `sources` (source + section + chunk_id) и `quotes`. Проверяется кодом: \
         источник указывает на фрагмент из контекста и chunk_id совпадает; цитата дословно есть в этом фрагменте; \
         числа ответа есть в цитатах. Смысл ответа против цитат — отдельный вызов LLM при temperature 0, который \
         видит только ответ и цитаты.\n\n## Сводка\n\n",
        settings.model,
        r.db.display(),
        r.strategy,
        r.len(),
        p.label(),
        idk.label()
    );
    md += &summary(rows);
    md += "\n## По вопросам\n\n| # | вопрос | итог | ожидание | попыток | tok ответа | tok этапов |\n|---|---|---|---|---|---|---|\n";
    for row in rows {
        let (verdict, attempts, tok) = match &row.card {
            Some(k) => (k.verdict(), k.attempts.to_string(), (k.prompt_tokens + k.completion_tokens).to_string()),
            None => (format!("ошибка: {}", row.error.as_deref().unwrap_or("")), "—".into(), "—".into()),
        };
        let cov = if row.must.is_empty() {
            if row.made_up.is_empty() { "—".into() } else { format!("выдумал: {}", row.made_up.join(", ")) }
        } else {
            format!("{}/{}", row.covered, row.must.len())
        };
        md += &format!("| {} | {} | {verdict} | {cov} | {attempts} | {tok} | {} |\n", row.id, row.q, row.stage_tokens);
    }
    md += "\n## Ответы\n";
    for row in rows {
        md += &format!("\n### {}. {}\n\n**Ожидание:** {}\n\n_{} · в контексте {} чанк._\n\n", row.id, row.q, row.expect, row.pipeline, row.context.len());
        for (i, c) in row.context.iter().enumerate() {
            md += &format!("{}. {c}\n", i + 1);
        }
        if !row.context.is_empty() {
            md += "\n";
        }
        let Some(k) = &row.card else {
            md += &format!("> ошибка: {}\n", row.error.as_deref().unwrap_or(""));
            continue;
        };
        if !k.retried.is_empty() {
            md += &format!("Повторы: {}\n\n", k.retried.join("; "));
        }
        for l in k.to_text().lines() {
            md += &format!("> {l}\n");
        }
        for q in k.quotes.iter().filter(|q| !q.found.ok()) {
            md += &format!("\n- ✗ цитата [{}] не найдена дословно ({:?})", q.n, q.found);
        }
        if let Some(s) = k.support.as_ref().filter(|s| !s.unsupported.is_empty()) {
            md += &format!("\n- судья: не подтверждено цитатами — {}", s.unsupported.join("; "));
        }
        md += "\n";
    }
    md
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rag::Chunk;

    fn hit(id: &str, text: &str, llm: Option<u8>, z: f32) -> Hit {
        Hit {
            chunk: Chunk {
                chunk_id: id.into(),
                strategy: "fixed".into(),
                source: "docs/raft.pdf".into(),
                file: "raft.pdf".into(),
                title: "Raft".into(),
                section: "Front matter".into(),
                sections: vec!["Front matter".into()],
                page_start: 6,
                page_end: 6,
                char_start: 0,
                char_end: text.len(),
                text: text.into(),
            },
            score: 0.99,
            z,
            llm,
            pos: 1,
            from_rewrite: false,
        }
    }

    const RAFT: &str = "Raft uses randomized election time-\nouts to ensure that split votes are rare. Election timeouts are chosen randomly from a fixed interval (e.g., 150–300ms).";

    #[test]
    fn quotes_are_found_through_pdf_line_breaks_and_misattribution_is_caught() {
        let hits = vec![hit("fixed-raft-0001", "Leaders send heartbeats.", Some(7), 3.0), hit("fixed-raft-0002", RAFT, Some(10), 4.0)];
        assert_eq!(locate("Raft uses randomized election timeouts to ensure that split votes are rare", 2, &hits), Found::Verbatim);
        assert_eq!(locate("“Election timeouts are chosen randomly from a fixed interval (e.g., 150-300ms)”", 2, &hits), Found::Verbatim);
        assert_eq!(locate("Raft uses randomized election timeouts", 1, &hits), Found::Elsewhere(2));
        assert_eq!(locate("Raft picks timeouts at random to avoid split votes", 2, &hits), Found::Missing);
        assert_eq!(locate("split votes", 2, &hits), Found::Short);
    }

    #[test]
    fn numbers_skip_citation_marks_and_names() {
        assert_eq!(numbers("Timeouts are 150–300 ms [1], [2,3]."), vec!["150", "300"]);
        assert_eq!(numbers("5,000 TPUs and 64 GPUs; GPT-4 rated it; 3.0 solar masses"), vec!["5000", "64", "30"]);
        assert!(numbers("no digits [1]").is_empty());
    }

    #[test]
    fn verify_resolves_sources_and_checks_quotes_and_numbers() {
        let hits = vec![hit("fixed-raft-0002", RAFT, Some(10), 4.0)];
        let reply = r#"Sure: ```json
{"status":"answer","answer":"Raft picks timeouts from 150–300 ms [1] so split votes are rare; 500 ms max.",
 "sources":[{"n":"[1]","source":"raft.pdf","section":"Front matter","chunk_id":"fixed-raft-0002"},{"n":7,"chunk_id":"nope"}],
 "quotes":[{"n":1,"text":"Election timeouts are chosen randomly from a fixed interval (e.g., 150–300ms)"},{"n":1,"text":"Raft never fails."}]}
```"#;
        let card = verify(reply, "q", &hits);
        let c = card.check.as_ref().unwrap();
        assert!(c.json_ok && c.has_sources() && c.has_quotes());
        assert_eq!((c.sources_ok, c.sources), (1, 2));
        assert_eq!((c.quotes_ok, c.quotes), (1, 2));
        assert_eq!(c.unbacked, vec!["500"]);
        assert!(!card.grounded());
        let p = problems(&card, &hits);
        assert!(p.iter().any(|x| x.contains("Raft never fails")) && p.iter().any(|x| x.contains("500")), "{p:?}");

        let good = r#"{"status":"answer","answer":"From 150–300 ms [1].","sources":[{"n":1,"section":"Front matter · стр. 6","chunk_id":"fixed-raft-0002"}],
                       "quotes":[{"n":1,"text":"chosen randomly from a fixed interval (e.g., 150–300ms)"}]}"#;
        let card = verify(good, "q", &hits);
        assert!(card.grounded() && problems(&card, &hits).is_empty());
        assert!(card.to_text().contains("[1] raft.pdf — Front matter, стр. 6 · fixed-raft-0002"));

        let swapped = r#"{"status":"answer","answer":"x [1]","sources":[{"n":2,"chunk_id":"fixed-raft-0002"}],"quotes":[]}"#;
        let two = vec![hit("fixed-raft-0001", "Leaders send heartbeats.", Some(7), 3.0), hits[0].clone()];
        let s = &verify(swapped, "q", &[hits[0].clone()]).sources[0];
        assert!(s.n == 1 && s.id_ok, "the id wins over a positional n");
        assert_eq!(verify(swapped, "q", &two).sources[0].n, 2);

        let prose = verify("Raft uses timeouts.", "q", &hits);
        assert!(!prose.check.as_ref().unwrap().json_ok && prose.answer == "Raft uses timeouts.");

        let unknown = verify(r#"{"status":"unknown","answer":"","sources":[],"quotes":[],"clarify":"Which system?"}"#, "q", &hits);
        assert_eq!((unknown.status, unknown.by), (Status::Unknown, Some(By::Model)));
        assert_eq!(unknown.clarify.as_deref(), Some("Which system?"));
        assert!(problems(&unknown, &hits).is_empty());
    }

    #[test]
    fn the_gate_says_unknown_below_the_threshold_and_names_the_nearest() {
        let idk = Idk { llm: 7, z: 3.0 };
        let strong = Retrieval { kept: vec![hit("a", RAFT, Some(9), 2.0)], ..Default::default() };
        assert_eq!(weak(&strong, idk), None);
        let partial = Retrieval { kept: vec![hit("a", RAFT, Some(5), 4.0)], ..Default::default() };
        assert!(weak(&partial, idk).unwrap().contains("llm 5 < порога 7"));
        let near_miss = Retrieval { pool: vec![hit("b", RAFT, Some(4), 3.1), hit("c", RAFT, None, 2.0)], ..Default::default() };
        assert!(weak(&near_miss, idk).unwrap().contains("llm 4"));
        let no_reranker = Retrieval { kept: vec![hit("d", RAFT, None, 2.8)], ..Default::default() };
        assert!(weak(&no_reranker, idk).unwrap().contains("z 2.80 < порога 3.0"));
        let card = Card::unknown("how fast?", &near_miss, weak(&near_miss, idk).unwrap());
        assert_eq!((card.status, card.by), (Status::Unknown, Some(By::Gate)));
        assert_eq!(card.nearest.len(), 1, "same file and section is listed once");
        let text = card.to_text();
        assert!(text.starts_with("Не знаю") && text.contains("Уточните") && text.contains("how fast?"));
        assert!(!text.contains("rewrite on"));
        assert!(Card::unknown("как быстро?", &near_miss, "x".into()).to_text().contains("/rag rewrite on"));
    }

    #[test]
    fn prompt_names_source_section_and_chunk_id_before_the_question() {
        let p = prompt("Why random?", &[hit("fixed-raft-0002", RAFT, Some(10), 4.0)], None);
        let header = p.find("[1] source=raft.pdf | section=Front matter | pages=стр. 6 | chunk_id=fixed-raft-0002").unwrap();
        assert!(p.find("\"status\":\"unknown\"").unwrap() < header && header < p.find("Question: Why random?").unwrap());
        assert!(prompt("q", &[], None).contains("no relevant excerpts"));
    }
}
