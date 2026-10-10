//! Task 23: the second stage after retrieval, and query rewriting.
//!
//! ```text
//! question ─┬─────────────────────────────► cosine with every chunk ─┐
//!           └─ [rewrite: LLM → search query] ► cosine with every chunk ┤ z-score per query,
//!                                                                      │ max of the two
//!   top-`pool` candidates (top-K before) ◄─────────────────────────────┘
//!     → [sim: drop z < min_z]            similarity threshold
//!     → [llm: LLM scores 0–10, drop < min_llm, sort by score]   reranker
//!     → top-`k` (top-K after) → the answer prompt
//! ```
//!
//! **Why a z-score and not the cosine.** The index stores min-max normalized
//! vectors (task 21); after the shift into `[0, 1]` every pair of vectors
//! shares a large positive part, and cosines collapse into 0.98–0.997 for
//! relevant and irrelevant chunks alike. The order survives, the absolute
//! value does not mean "similar". So the threshold is on how far a chunk
//! stands out from the whole corpus *for this query*: `z = (cos − mean) /
//! std` over all searchable chunks. It is scale-free and the same number
//! means the same thing for a terse query and a long one.
//!
//! **The reranker** is a separate LLM call: the original question and every
//! surviving candidate, one JSON score per passage (10 — states the answer,
//! 0 — unrelated). It judges the user's question, not the rewritten one:
//! the rewrite only serves the search. Scores are cached per (question,
//! chunk), so modes that share candidates are judged once and consistently;
//! a cached judgement is still charged to the mode that uses it (its share
//! of the call's tokens), so the cost columns compare modes as if each ran
//! alone.

use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use crate::agent::Agent;
use crate::api::ChatMessage;
use crate::config::{Res, Settings};
use crate::ragqa::{expected, expected_file, expected_rank, load_controls, Control, Hit, Retriever};

pub const DEFAULT_FILTER: &str = "both";
/// Top-K before the filter. Tuned by `ask --rag-tune` (see README).
pub const DEFAULT_POOL: usize = 20;
pub const MAX_POOL: usize = 50;
/// Similarity threshold, z-score of the cosine. Tuned by `ask --rag-tune`.
pub const DEFAULT_MIN_Z: f32 = 2.5;
/// Reranker threshold, 0–10. Tuned by `ask --rag-tune`.
pub const DEFAULT_MIN_LLM: u8 = 5;
/// Characters of each passage shown to the reranker (fixed chunks are ≤ 1000).
const PASSAGE_CHARS: usize = 1500;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Filter {
    Off,
    Sim,
    Llm,
    Both,
}

impl Filter {
    pub fn parse(s: &str) -> Res<Filter> {
        match s.trim() {
            "off" | "none" => Ok(Filter::Off),
            "sim" | "similarity" | "threshold" => Ok(Filter::Sim),
            "llm" | "rerank" => Ok(Filter::Llm),
            "both" | "sim+llm" => Ok(Filter::Both),
            other => Err(format!("фильтр {other:?}: off | sim | llm | both")),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Filter::Off => "off",
            Filter::Sim => "sim",
            Filter::Llm => "llm",
            Filter::Both => "both",
        }
    }

    pub fn sim(self) -> bool {
        matches!(self, Filter::Sim | Filter::Both)
    }

    pub fn llm(self) -> bool {
        matches!(self, Filter::Llm | Filter::Both)
    }

    /// Next value for the settings panel.
    pub fn cycle(self) -> Filter {
        match self {
            Filter::Off => Filter::Sim,
            Filter::Sim => Filter::Llm,
            Filter::Llm => Filter::Both,
            Filter::Both => Filter::Off,
        }
    }
}

/// Everything that decides which chunks reach the model.
#[derive(Clone, Debug, Serialize)]
pub struct Pipeline {
    pub rewrite: bool,
    pub filter: Filter,
    /// Candidates taken before the filter (top-K before).
    pub pool: usize,
    /// Chunks handed to the model at most (top-K after).
    pub k: usize,
    pub min_z: f32,
    pub min_llm: u8,
}

impl Pipeline {
    pub fn from_settings(s: &Settings) -> Pipeline {
        Pipeline {
            rewrite: s.rag_rewrite,
            filter: Filter::parse(&s.rag_filter).unwrap_or(Filter::Off),
            pool: s.rag_pool.max(s.rag_k),
            k: s.rag_k,
            min_z: s.rag_min_sim,
            min_llm: s.rag_min_llm,
        }
    }

    /// Task 22's retrieval: top-k by cosine, nothing else.
    pub fn is_base(&self) -> bool {
        !self.rewrite && self.filter == Filter::Off
    }

    pub fn needs_llm(&self) -> bool {
        self.rewrite || self.filter.llm()
    }

    /// `rewrite + sim z≥3.0 + llm ≥5 · 20→4`
    pub fn label(&self) -> String {
        let mut parts = Vec::new();
        if self.rewrite {
            parts.push("rewrite".to_string());
        }
        if self.filter.sim() {
            parts.push(format!("sim z≥{:.1}", self.min_z));
        }
        if self.filter.llm() {
            parts.push(format!("llm ≥{}", self.min_llm));
        }
        if parts.is_empty() {
            format!("без фильтра · top-{}", self.k)
        } else if self.filter == Filter::Off {
            format!("{} · top-{}", parts.join(" + "), self.k)
        } else {
            format!("{} · {}→{}", parts.join(" + "), self.pool, self.k)
        }
    }
}

/// The LLM side of the pipeline: rewriting and reranking, with a score
/// cache and token accounting.
pub struct Judge {
    agent: Agent,
    /// (question, chunk_id) → score and its share of the call's tokens.
    cache: Mutex<HashMap<(String, String), (u8, Cost)>>,
    /// question → rewritten query and the call's tokens.
    rewrites: Mutex<HashMap<String, (String, Cost)>>,
    /// Titles of the indexed documents, shown to the rewriter.
    catalog: String,
}

const JUDGE_SYSTEM: &str = "You are a component of a document retrieval system. Follow the output format exactly.";

impl Judge {
    pub fn new(settings: &Settings, retriever: &Retriever) -> Res<Judge> {
        let mut s = settings.clone();
        s.system_prompt = JUDGE_SYSTEM.into();
        s.context_enabled = false;
        s.invariants = false;
        s.todo = false;
        s.rag = false;
        s.profile = crate::profile::OFF.into();
        s.json_mode.enabled = false;
        s.max_chars = None;
        s.temperature = Some(0.0);
        let catalog = retriever.titles().iter().map(|(f, t)| format!("- {f}: {t}")).collect::<Vec<_>>().join("\n");
        Ok(Judge { agent: Agent::new(s)?, cache: Mutex::new(HashMap::new()), rewrites: Mutex::new(HashMap::new()), catalog })
    }

    fn call(&self, prompt: &str) -> Res<(String, Cost)> {
        let o = self.agent.complete_outcome(&[ChatMessage::user(prompt)])?;
        let cost = Cost { prompt: o.usage.prompt_tokens as f64, completion: o.usage.completion_tokens as f64 };
        Ok((o.text().trim().to_string(), cost))
    }

    /// The question as a search query: abbreviations expanded, terse
    /// fragments made into a full question, key terms of the likely answer
    /// passage added. English, one line.
    pub fn rewrite(&self, question: &str, stats: &mut Retrieval) -> Res<String> {
        stats.llm_calls += 1;
        if let Some((q, cost)) = self.rewrites.lock().map_err(|e| e.to_string())?.get(question.trim()) {
            cost.charge(stats);
            return Ok(q.clone());
        }
        let prompt = format!(
            "Rewrite the user's question into one search query for a semantic search index over these documents:\n\
             {}\n\n\
             Rules: write it in English as a complete, specific question; expand abbreviations and terse \
             fragments; name the system, paper or concept explicitly; add 3–8 key terms that the passage \
             containing the answer is likely to use; you may say what a term or abbreviation means. Do not answer \
             the question: never add the facts, numbers or names you expect in the answer. Reply with the query \
             only, on one line, no quotes.\n\nQuestion: {}",
            self.catalog,
            question.trim()
        );
        let (text, cost) = self.call(&prompt)?;
        cost.charge(stats);
        let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
        let line = line.trim_matches(|c| c == '"' || c == '“' || c == '”').trim();
        if line.is_empty() {
            return Err("rewrite: пустой ответ".into());
        }
        self.rewrites.lock().map_err(|e| e.to_string())?.insert(question.trim().to_string(), (line.to_string(), cost));
        Ok(line.to_string())
    }

    /// Scores 0–10 for every candidate, in order; one call for the ones not
    /// in the cache yet.
    pub fn scores(&self, question: &str, cands: &[Hit], stats: &mut Retrieval) -> Res<Vec<u8>> {
        let key = |h: &Hit| (question.trim().to_string(), h.chunk.chunk_id.clone());
        let todo: Vec<&Hit> = {
            let cache = self.cache.lock().map_err(|e| e.to_string())?;
            cands.iter().filter(|h| !cache.contains_key(&key(h))).collect()
        };
        stats.llm_calls += 1;
        if !todo.is_empty() {
            let (text, cost) = self.call(&rerank_prompt(question, &todo))?;
            let share = Cost { prompt: cost.prompt / todo.len() as f64, completion: cost.completion / todo.len() as f64 };
            let got = parse_scores(&text, todo.len())
                .ok_or_else(|| format!("реранкер вернул не JSON с оценками: {}", text.chars().take(200).collect::<String>()))?;
            let mut cache = self.cache.lock().map_err(|e| e.to_string())?;
            let mut missing = 0;
            for (i, h) in todo.iter().enumerate() {
                let s = got.get(&(i + 1)).copied();
                missing += s.is_none() as usize;
                cache.insert(key(h), (s.unwrap_or(0), share));
            }
            if missing > 0 {
                stats.warnings.push(format!("реранкер не оценил {missing} из {} — считаю 0", todo.len()));
            }
        }
        let cache = self.cache.lock().map_err(|e| e.to_string())?;
        let mut total = Cost::default();
        let scores = cands
            .iter()
            .map(|h| {
                let (s, c) = cache.get(&key(h)).copied().unwrap_or_default();
                total.prompt += c.prompt;
                total.completion += c.completion;
                s
            })
            .collect();
        total.charge(stats);
        Ok(scores)
    }
}

/// Tokens of one LLM call (or a share of one).
#[derive(Clone, Copy, Debug, Default)]
struct Cost {
    prompt: f64,
    completion: f64,
}

impl Cost {
    fn charge(self, stats: &mut Retrieval) {
        stats.prompt_tokens += self.prompt.round() as u64;
        stats.completion_tokens += self.completion.round() as u64;
    }
}

pub fn rerank_prompt(question: &str, cands: &[&Hit]) -> String {
    let mut s = format!(
        "Question: {}\n\n\
         Rate how useful each passage below is for answering this question, on a scale 0–10:\n\
         10 = states the answer directly; 7 = contains a substantial part of the answer; \
         4 = same topic or document but not the answer; 0 = unrelated.\n\
         Judge only what the passage says, not your own knowledge. Reply with JSON only, one entry per passage:\n\
         {{\"scores\": [{{\"id\": 1, \"score\": 7}}, …]}}\n\n",
        question.trim()
    );
    for (i, h) in cands.iter().enumerate() {
        let text: String = h.chunk.text.chars().take(PASSAGE_CHARS).collect();
        s += &format!("<passage id=\"{}\" source=\"{} — {}\">\n{}\n</passage>\n\n", i + 1, h.chunk.file, h.cite(), text.trim());
    }
    s
}

/// `{"scores":[{"id":1,"score":7},…]}`, `{"1":7,…}` or a bare array, maybe
/// fenced or with prose around it. `None` — nothing usable at all.
pub fn parse_scores(text: &str, n: usize) -> Option<HashMap<usize, u8>> {
    let start = text.find(['{', '['])?;
    let end = text.rfind(['}', ']'])?;
    let v: Value = serde_json::from_str(text.get(start..=end)?).ok()?;
    let num = |v: &Value| v.as_f64().or_else(|| v.as_str()?.trim().parse().ok()).map(|f| f.round().clamp(0.0, 10.0) as u8);
    let mut out = HashMap::new();
    let list = match &v {
        Value::Array(a) => Some(a.clone()),
        Value::Object(o) => o.get("scores").and_then(|s| s.as_array().cloned()),
        _ => None,
    };
    match (list, &v) {
        (Some(items), _) => {
            for (i, it) in items.iter().enumerate() {
                match it {
                    Value::Object(o) => {
                        let id = o.get("id").and_then(|x| x.as_u64().or_else(|| x.as_str()?.parse().ok())).map(|x| x as usize).unwrap_or(i + 1);
                        if let Some(s) = o.get("score").and_then(num) {
                            out.insert(id, s);
                        }
                    }
                    other => {
                        if let Some(s) = num(other) {
                            out.insert(i + 1, s);
                        }
                    }
                }
            }
        }
        (None, Value::Object(o)) => {
            for (k, x) in o {
                if let (Ok(id), Some(s)) = (k.trim().parse::<usize>(), num(x)) {
                    out.insert(id, s);
                }
            }
        }
        _ => {}
    }
    out.retain(|id, _| (1..=n).contains(id));
    (!out.is_empty()).then_some(out)
}

/// What one question went through: the query, the candidates and what
/// survived, with the cost of the extra stages.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Retrieval {
    pub rewritten: Option<String>,
    /// Candidates before the filter, in retrieval order.
    pub pool: Vec<Hit>,
    /// What goes to the model, in final order.
    pub kept: Vec<Hit>,
    pub dropped_sim: usize,
    pub dropped_llm: usize,
    /// LLM stages run (rewrite, rerank) — cached ones included.
    pub llm_calls: usize,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub ms: u128,
    pub warnings: Vec<String>,
}

/// Mean and standard deviation of `xs`.
fn mean_std(xs: &[f32]) -> (f32, f32) {
    let n = xs.len().max(1) as f32;
    let mean = xs.iter().sum::<f32>() / n;
    let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f32>() / n;
    (mean, var.sqrt().max(1e-9))
}

/// `(cosine, z)` for every searchable chunk (`None` — skipped section).
pub fn zscores(cos: &[Option<f32>]) -> Vec<Option<(f32, f32)>> {
    let vals: Vec<f32> = cos.iter().flatten().copied().collect();
    let (mean, std) = mean_std(&vals);
    cos.iter().map(|c| c.map(|c| (c, (c - mean) / std))).collect()
}

/// Candidates by z, best first: one query, or the max over two (original
/// and rewritten) per chunk. `from_rewrite` marks chunks the rewrite scored
/// higher.
pub fn candidates(r: &Retriever, scored: &[Vec<Option<(f32, f32)>>], n: usize) -> Vec<Hit> {
    let mut best: Vec<(usize, f32, f32, bool)> = Vec::new();
    for i in 0..r.len() {
        let mut top: Option<(f32, f32, bool)> = None;
        for (q, s) in scored.iter().enumerate() {
            if let Some((c, z)) = s[i] {
                if top.is_none_or(|t| z > t.1) {
                    top = Some((c, z, q > 0));
                }
            }
        }
        if let Some((c, z, rw)) = top {
            best.push((i, c, z, rw));
        }
    }
    best.sort_by(|a, b| b.2.total_cmp(&a.2));
    best.into_iter()
        .take(n)
        .enumerate()
        .map(|(pos, (i, c, z, rw))| Hit { chunk: r.chunk(i).clone(), score: c, z, llm: None, pos: pos + 1, from_rewrite: rw })
        .collect()
}

/// The whole pipeline for one question.
pub fn run(r: &Retriever, p: &Pipeline, judge: Option<&Judge>, question: &str) -> Res<Retrieval> {
    let t = Instant::now();
    let mut out = Retrieval::default();
    let need = |what: &str| format!("{what} нужен LLM, а судьи нет");
    let mut scored = vec![zscores(&r.cosines(question)?)];
    if p.rewrite {
        let j = judge.ok_or_else(|| need("rewrite"))?;
        match j.rewrite(question, &mut out) {
            Ok(q) => {
                scored.push(zscores(&r.cosines(&q)?));
                out.rewritten = Some(q);
            }
            Err(e) => out.warnings.push(format!("rewrite не удался ({e}) — ищу по исходному вопросу")),
        }
    }
    let pool_n = if p.filter == Filter::Off { p.k } else { p.pool.max(p.k) };
    out.pool = candidates(r, &scored, pool_n);
    let mut applied = p.clone();
    if p.filter.llm() {
        let judged: Vec<Hit> = out.pool.iter().filter(|h| !p.filter.sim() || h.z >= p.min_z).cloned().collect();
        if !judged.is_empty() {
            let j = judge.ok_or_else(|| need("реранкеру"))?;
            match j.scores(question, &judged, &mut out) {
                Ok(scores) => {
                    for h in out.pool.iter_mut() {
                        h.llm = judged.iter().position(|x| x.chunk.chunk_id == h.chunk.chunk_id).map(|i| scores[i]);
                    }
                }
                Err(e) => {
                    out.warnings.push(format!("реранкер не сработал ({e}) — оставляю порядок поиска"));
                    applied.filter = if p.filter.sim() { Filter::Sim } else { Filter::Off };
                }
            }
        }
    }
    let (kept, dropped_sim, dropped_llm) = select(&out.pool, &applied);
    (out.kept, out.dropped_sim, out.dropped_llm) = (kept, dropped_sim, dropped_llm);
    out.ms = t.elapsed().as_millis();
    Ok(out)
}

/// The second stage over an already scored pool (retrieval order, `llm`
/// set where the reranker ran): similarity threshold, reranker threshold
/// and order, top-`k`. Returns what is kept and how many each threshold cut.
pub fn select(pool: &[Hit], p: &Pipeline) -> (Vec<Hit>, usize, usize) {
    let n = if p.filter == Filter::Off { p.k } else { p.pool.max(p.k) };
    let mut kept: Vec<Hit> = pool.iter().take(n).cloned().collect();
    let (mut dropped_sim, mut dropped_llm) = (0, 0);
    if p.filter.sim() {
        let before = kept.len();
        kept.retain(|h| h.z >= p.min_z);
        dropped_sim = before - kept.len();
    }
    if p.filter.llm() {
        let before = kept.len();
        kept.retain(|h| h.llm.unwrap_or(0) >= p.min_llm);
        dropped_llm = before - kept.len();
        // stable: equal scores keep the retrieval order
        kept.sort_by(|a, b| b.llm.cmp(&a.llm));
    }
    kept.truncate(p.k);
    (kept, dropped_sim, dropped_llm)
}

// ---------------------------------------------------------------- tuning

/// One control question, scored once: the candidates of the original query
/// and of original + rewrite, with reranker scores on the top of both.
struct Probe {
    c: Control,
    rewritten: String,
    orig: Vec<Hit>,
    merged: Vec<Hit>,
}

/// Retrieval quality of one pipeline over the control set.
#[derive(Default)]
struct Score {
    /// Answerable questions with an expected chunk in the context.
    hit: usize,
    /// Answerable questions whose context the filter emptied.
    lost: usize,
    kept: usize,
    /// Kept chunks from documents that do not hold the answer.
    noise: usize,
    /// Unanswerable questions left with an empty context.
    empty: usize,
}

fn score(probes: &[Probe], p: &Pipeline, merged: bool) -> Score {
    let mut s = Score::default();
    for pr in probes {
        let pool = if merged { &pr.merged } else { &pr.orig };
        let (kept, _, _) = select(pool, p);
        s.kept += kept.len();
        s.noise += kept.iter().filter(|h| !expected_file(h, &pr.c)).count();
        if pr.c.answerable() {
            s.hit += kept.iter().any(|h| expected(h, &pr.c)) as usize;
            s.lost += kept.is_empty() as usize;
        } else {
            s.empty += kept.is_empty() as usize;
        }
    }
    s
}

fn score_row(label: &str, s: &Score, answerable: usize, none: usize) -> String {
    format!(
        "| {label} | {}/{answerable} | {} | {} | {}/{none} | {}/{answerable} |\n",
        s.hit, s.kept, s.noise, s.empty, s.lost
    )
}

const SCORE_HEAD: &str = "| настройка | нужный чанк в контексте | чанков Σ | из чужих документов Σ | без ответа: пустой контекст | с ответом: контекст опустел |\n|---|---|---|---|---|---|\n";

/// `ask --rag-tune`: no answers, only retrieval and the reranker — how many
/// candidates to take (top-K before), where to put the similarity and the
/// reranker thresholds. Writes `rag/tune.md`.
pub fn tune(settings: &Settings, paths: &crate::rag::Config, only: &[usize]) -> Res<()> {
    let controls: Vec<Control> =
        load_controls(&paths.dir)?.into_iter().filter(|c| only.is_empty() || only.contains(&c.id)).collect();
    let r = Retriever::open(&paths.db, &settings.rag_strategy, &paths.url, settings.rag_provider)?;
    let judge = Judge::new(settings, &r)?;
    let base = Pipeline::from_settings(settings);
    let judged = base.pool.max(base.k);
    println!(
        "тюнинг: {} вопросов · индекс {} ({}, {} чанков) · реранкер {} на top-{judged}",
        controls.len(),
        r.db.display(),
        r.strategy,
        r.len(),
        settings.model
    );
    let mut probes = Vec::new();
    let mut stats = Retrieval::default();
    for c in controls {
        let z_orig = zscores(&r.cosines(&c.q)?);
        let rewritten = judge.rewrite(&c.q, &mut stats)?;
        let z_rw = zscores(&r.cosines(&rewritten)?);
        let mut orig = candidates(&r, std::slice::from_ref(&z_orig), MAX_POOL);
        let mut merged = candidates(&r, &[z_orig, z_rw], MAX_POOL);
        for pool in [&mut orig, &mut merged] {
            let top: Vec<Hit> = pool.iter().take(judged).cloned().collect();
            let scores = judge.scores(&c.q, &top, &mut stats)?;
            for (h, s) in pool.iter_mut().zip(scores) {
                h.llm = Some(s);
            }
        }
        println!(
            "[{}] нужный на {} (rewrite: {}) · {}",
            c.id,
            show_rank(expected_rank(&orig, &c)),
            show_rank(expected_rank(&merged, &c)),
            rewritten
        );
        probes.push(Probe { c, rewritten, orig, merged });
    }
    let md = tune_report(settings, &r, &base, &probes, &stats);
    let path = paths.db.parent().map(|d| d.join("tune.md")).unwrap_or_else(|| "tune.md".into());
    std::fs::write(&path, &md).map_err(|e| format!("{}: {e}", path.display()))?;
    println!("\n{md}\nотчёт: {}", path.display());
    Ok(())
}

fn show_rank(r: usize) -> String {
    if r == 0 { "—".into() } else { r.to_string() }
}

fn tune_report(settings: &Settings, r: &Retriever, base: &Pipeline, probes: &[Probe], stats: &Retrieval) -> String {
    let answerable = probes.iter().filter(|p| p.c.answerable()).count();
    let none = probes.len() - answerable;
    let judged = base.pool.max(base.k);
    let mut md = format!(
        "# Настройка второго этапа (задача 23)\n\n\
         Индекс `{}` (`{}`, {} чанков, {} документов), вопросы — `docs/control.json`: {answerable} с ответом, {none} без. \
         Реранкер и rewrite — `{}`, temperature 0; реранкер оценил top-{judged} кандидатов каждого вопроса \
         ({} этапов LLM, ≈{} tok — повторные оценки берутся из кэша, но засчитываются по доле). Ответы здесь не генерируются — только то, что попало бы в контекст.\n\n",
        r.db.display(),
        r.strategy,
        r.len(),
        r.titles().len(),
        settings.model,
        stats.llm_calls,
        stats.prompt_tokens + stats.completion_tokens
    );
    // 1. top-K before
    md += "## 1. Сколько кандидатов брать (top-K до)\n\nRecall@N — у скольких вопросов с ответом нужный чанк (файл + страница) есть среди первых N по косинусу.\n\n| N | 1 | 4 | 10 | 20 | 30 | 50 |\n|---|---|---|---|---|---|---|\n";
    for (label, merged) in [("исходный вопрос", false), ("вопрос + rewrite", true)] {
        md += &format!("| {label} |");
        for n in [1, 4, 10, 20, 30, 50] {
            let hit = probes
                .iter()
                .filter(|p| p.c.answerable())
                .filter(|p| {
                    let r = expected_rank(if merged { &p.merged } else { &p.orig }, &p.c);
                    r > 0 && r <= n
                })
                .count();
            md += &format!(" {hit}/{answerable} |");
        }
        md += "\n";
    }
    // 2. similarity threshold
    md += &format!(
        "\n## 2. Порог similarity (z-скор косинуса)\n\ntop-{} кандидатов → z ≥ порога → top-{}. Исходный вопрос, без реранкера.\n\n{SCORE_HEAD}",
        base.pool, base.k
    );
    let at = |filter: Filter, min_z: f32, min_llm: u8, pool: usize| Pipeline { rewrite: false, filter, pool, k: base.k, min_z, min_llm };
    md += &score_row("без порога (base)", &score(probes, &at(Filter::Off, 0.0, 0, base.pool), false), answerable, none);
    for z in [1.0, 1.5, 2.0, 2.5, 3.0, 3.5, 4.0, 4.5] {
        md += &score_row(&format!("z ≥ {z:.1}"), &score(probes, &at(Filter::Sim, z, 0, base.pool), false), answerable, none);
    }
    // 3. reranker threshold
    md += &format!(
        "\n## 3. Порог реранкера (LLM, 0–10)\n\ntop-{judged} кандидатов → LLM-оценка ≥ порога → сортировка по оценке → top-{}. Исходный вопрос.\n\n{SCORE_HEAD}",
        base.k
    );
    for t in 0..=9u8 {
        md += &score_row(&format!("llm ≥ {t}"), &score(probes, &at(Filter::Llm, 0.0, t, judged), false), answerable, none);
    }
    // 4. top-K before for the reranker
    md += &format!("\n## 4. top-K до для реранкера\n\nПорог llm ≥ {}, после — top-{}.\n\n{SCORE_HEAD}", base.min_llm, base.k);
    for n in [base.k, 10, judged].into_iter().filter(|n| *n <= judged).collect::<std::collections::BTreeSet<_>>() {
        md += &score_row(&format!("top-{n} → top-{}", base.k), &score(probes, &at(Filter::Llm, 0.0, base.min_llm, n), false), answerable, none);
    }
    // 5. combinations with rewrite
    md += &format!("\n## 5. Вместе с rewrite\n\nКандидаты — лучший z из исходного вопроса и переписанного; top-{judged} → фильтры → top-{}.\n\n{SCORE_HEAD}", base.k);
    let rw = |filter: Filter, min_z: f32, min_llm: u8| Pipeline { rewrite: true, filter, pool: judged, k: base.k, min_z, min_llm };
    md += &score_row("rewrite, без фильтра", &score(probes, &rw(Filter::Off, 0.0, 0), true), answerable, none);
    for z in [2.0, 2.5, 3.0] {
        md += &score_row(&format!("rewrite + z ≥ {z:.1}"), &score(probes, &rw(Filter::Sim, z, 0), true), answerable, none);
    }
    for t in [3u8, 5, 7] {
        md += &score_row(&format!("rewrite + llm ≥ {t}"), &score(probes, &rw(Filter::Llm, 0.0, t), true), answerable, none);
        for z in [2.0, 2.5] {
            md += &score_row(&format!("rewrite + z ≥ {z:.1} + llm ≥ {t}"), &score(probes, &rw(Filter::Both, z, t), true), answerable, none);
        }
    }
    // 6. per question
    md += "\n## 6. По вопросам\n\nРанг — позиция первого нужного чанка среди 50 кандидатов. z — z-скор top-1 и нужного чанка; llm — оценка нужного чанка реранкером (у вопросов без ответа — лучшая оценка среди кандидатов).\n\n| # | вопрос | ранг | ранг с rewrite | z top-1 | z нужного | llm нужного / max | запрос после rewrite |\n|---|---|---|---|---|---|---|---|\n";
    for p in probes {
        let first = p.orig.iter().find(|h| expected(h, &p.c));
        let best_llm = p.merged.iter().filter(|h| expected(h, &p.c)).filter_map(|h| h.llm).max();
        let max_llm = p.merged.iter().filter_map(|h| h.llm).max().unwrap_or(0);
        md += &format!(
            "| {} | {} | {} | {} | {:.2} | {} | {} | {} |\n",
            p.c.id,
            p.c.q,
            show_rank(expected_rank(&p.orig, &p.c)),
            show_rank(expected_rank(&p.merged, &p.c)),
            p.orig.first().map(|h| h.z).unwrap_or(0.0),
            first.map(|h| format!("{:.2}", h.z)).unwrap_or_else(|| "—".into()),
            if p.c.answerable() {
                format!("{} / {max_llm}", best_llm.map(|s| s.to_string()).unwrap_or_else(|| "—".into()))
            } else {
                format!("— / {max_llm}")
            },
            p.rewritten.replace('|', "/")
        );
    }
    md
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_parses_and_cycles() {
        assert_eq!(Filter::parse("rerank").unwrap(), Filter::Llm);
        assert_eq!(Filter::parse("sim+llm").unwrap(), Filter::Both);
        assert!(Filter::parse("bm25").is_err());
        let mut f = Filter::Off;
        for _ in 0..4 {
            f = f.cycle();
        }
        assert_eq!(f, Filter::Off);
        assert!(Filter::Both.sim() && Filter::Both.llm() && !Filter::Sim.llm());
    }

    #[test]
    fn scores_parse_from_several_shapes() {
        let a = parse_scores("```json\n{\"scores\":[{\"id\":1,\"score\":7},{\"id\":2,\"score\":\"3\"}]}\n```", 2).unwrap();
        assert_eq!((a[&1], a[&2]), (7, 3));
        let b = parse_scores("Here: {\"1\": 9, \"2\": 0, \"9\": 5}", 2).unwrap();
        assert_eq!((b[&1], b[&2], b.len()), (9, 0, 2));
        let c = parse_scores("[4, 12.6]", 2).unwrap();
        assert_eq!((c[&1], c[&2]), (4, 10));
        assert!(parse_scores("no json here", 3).is_none());
    }

    #[test]
    fn zscores_stand_out_from_a_compressed_scale() {
        // cosines squeezed into 0.98–0.997 like the min-max index produces
        let mut cos: Vec<Option<f32>> = (0..100).map(|i| Some(0.985 + (i % 10) as f32 * 0.0002)).collect();
        cos[7] = Some(0.996);
        cos[3] = None;
        let z = zscores(&cos);
        assert!(z[3].is_none());
        assert!(z[7].unwrap().1 > 5.0, "the outlier stands out: {:?}", z[7]);
        assert!(z[0].unwrap().1 < 0.0);
    }

    fn hit(id: &str, z: f32, llm: Option<u8>, pos: usize) -> Hit {
        let chunk = crate::rag::Chunk {
            chunk_id: id.into(),
            strategy: "fixed".into(),
            source: "docs/x.pdf".into(),
            file: "x.pdf".into(),
            title: "X".into(),
            section: "Front matter".into(),
            sections: vec![],
            page_start: 1,
            page_end: 1,
            char_start: 0,
            char_end: 1,
            text: id.into(),
        };
        Hit { chunk, score: 0.99, z, llm, pos, from_rewrite: false }
    }

    #[test]
    fn select_applies_thresholds_then_reorders_and_cuts() {
        let pool = vec![
            hit("a", 4.0, Some(2), 1),
            hit("b", 3.5, Some(9), 2),
            hit("c", 3.0, Some(6), 3),
            hit("d", 2.0, Some(10), 4),
            hit("e", 2.8, Some(9), 5),
        ];
        let ids = |v: &[Hit]| v.iter().map(|h| h.chunk.chunk_id.clone()).collect::<Vec<_>>().join("");
        let mut p = Pipeline { rewrite: false, filter: Filter::Off, pool: 5, k: 2, min_z: 2.5, min_llm: 5 };
        // off: top-k in retrieval order, nothing judged or cut
        assert_eq!(ids(&select(&pool, &p).0), "ab");
        p.filter = Filter::Sim;
        let (kept, ds, dl) = select(&pool, &p);
        assert_eq!((ids(&kept), ds, dl), ("ab".into(), 1, 0));
        p.filter = Filter::Llm;
        let (kept, ds, dl) = select(&pool, &p);
        assert_eq!((ids(&kept), ds, dl), ("db".into(), 0, 1), "sorted by llm, ties keep retrieval order");
        p.filter = Filter::Both;
        let (kept, ds, dl) = select(&pool, &p);
        assert_eq!((ids(&kept), ds, dl), ("be".into(), 1, 1));
        p.min_llm = 10;
        assert!(select(&pool, &p).0.is_empty(), "nothing relevant → empty context");
    }

    #[test]
    fn pipeline_label_names_every_stage() {
        let mut p = Pipeline { rewrite: false, filter: Filter::Off, pool: 20, k: 4, min_z: 2.5, min_llm: 5 };
        assert!(p.is_base() && !p.needs_llm());
        assert_eq!(p.label(), "без фильтра · top-4");
        p.rewrite = true;
        assert_eq!(p.label(), "rewrite · top-4");
        p.filter = Filter::Both;
        assert_eq!(p.label(), "rewrite + sim z≥2.5 + llm ≥5 · 20→4");
        assert!(p.needs_llm());
    }
}
