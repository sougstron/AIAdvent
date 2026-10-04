//! Task 22: the first RAG request.
//!
//! question → [`Retriever::search`] (embedded by the Ollama model that built
//! the index, min-max normalized like the stored vectors, cosine against
//! every chunk of one strategy) → [`augment`] (the top-k chunks with file,
//! section and pages, then the question) → the LLM.
//!
//! The chat does it before every turn while the `rag` setting is on
//! (`/rag on`, `--rag`); the augmented text goes to the wire only, the
//! session keeps the question as typed. [`eval`] runs the control set
//! `docs/control.json` in up to three modes on the same model and the same
//! question:
//!
//! * `plain` — the model alone, no index, no tools;
//! * `rag` — the question augmented with the retrieved chunks;
//! * `mcp` — no index: the model reads the raw document itself through
//!   `ask-docs-mcp` (`docs_list` / `docs_read` / `docs_search`).
//!
//! Task 23 puts a second stage between the search and the prompt
//! (`rerank.rs`: query rewrite, similarity threshold, LLM reranker) and
//! runs the control set — now 20 questions over ten long PDFs, three of
//! them with no answer in the corpus — in modes `base` / `sim` / `llm` /
//! `rewrite` / `full`, so the improved pipeline is compared with the plain
//! one on the same questions.
//!
//! Every control question says what the answer must contain (`must`, groups
//! of alternatives matched at word starts) and which section holds it
//! (`sources`), so the comparison is counted, not eyeballed: coverage of the
//! expectation per mode, whether retrieval found the expected section (and
//! at which rank), whether the retrieved text itself contained the expected
//! terms (retrieval miss vs generation miss), tokens and time.

use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::agent::Agent;
use crate::api::ChatMessage;
use crate::config::{Res, Settings};
use crate::rag::{self, Chunk, Embedder};
use crate::rerank::{self, Filter, Judge, Pipeline, Retrieval};

pub const DEFAULT_K: usize = 4;
pub const MAX_K: usize = 12;
/// Ten papers of different layouts: the IEEE heading parser finds sections
/// in only two of them, so `structure` would be mostly 4000-character slabs
/// of "Front matter" — fixed 1000/200 windows are the default since task 23.
pub const DEFAULT_STRATEGY: &str = "fixed";
pub const DEFAULT_DOCS: &str = "docs";
pub const CONTROL_FILE: &str = "control.json";
/// The bibliography names every method of the survey and wins similarity
/// contests it holds no answer for (task 21's probe saw it outrank the
/// right section) — it is never handed to the model as context.
const SKIP_SECTIONS: [&str; 1] = ["REFERENCES"];

// ---------------------------------------------------------------- retrieval

#[derive(Clone, Debug, Serialize)]
pub struct Hit {
    pub chunk: Chunk,
    /// Cosine with the query (min-max normalized vectors — compressed scale).
    pub score: f32,
    /// How far the cosine stands out from all chunks for this query
    /// (`rerank::zscores`); 0 where nobody computed it.
    pub z: f32,
    /// Reranker score 0–10, when the LLM reranker ran.
    pub llm: Option<u8>,
    /// 1-based rank in the retrieval order (before any reranking).
    pub pos: usize,
    /// The rewritten query scored this chunk higher than the original.
    pub from_rewrite: bool,
}

impl Hit {
    /// `III. RETRIEVAL > D. Embedding, стр. 9`
    pub fn cite(&self) -> String {
        let c = &self.chunk;
        let pages = if c.page_start == c.page_end {
            format!("стр. {}", c.page_start)
        } else {
            format!("стр. {}–{}", c.page_start, c.page_end)
        };
        format!("{}, {pages}", c.section)
    }
}

pub struct Retriever {
    pub db: PathBuf,
    pub strategy: String,
    pub model: String,
    embedder: Embedder,
    chunks: Vec<Chunk>,
    embs: Vec<Vec<f32>>,
}

impl Retriever {
    /// Load one strategy of the index. Refuses an index that is missing,
    /// lacks the strategy or holds unnormalized vectors — a query vector
    /// scaled differently from the documents would rank nonsense quietly.
    pub fn open(db: &Path, strategy: &str, url: &str) -> Res<Retriever> {
        if !db.exists() {
            return Err(format!("{}: индекса нет — сначала ask --rag-index docs", db.display()));
        }
        let conn = rag::open(db)?;
        let Some((run, chunks, embs)) = rag::load_run(&conn, strategy)? else {
            return Err(format!(
                "{}: стратегии {strategy} в индексе нет — ask --rag-index docs --chunk-strategy {strategy}",
                db.display()
            ));
        };
        let (lo, hi) = rag::range(&embs);
        if lo < 0.0 || hi > 1.0 {
            return Err(format!("{}: векторы не нормализованы — ask --rag-index docs", db.display()));
        }
        Ok(Retriever {
            db: db.to_path_buf(),
            strategy: strategy.to_string(),
            embedder: Embedder::new(url, &run.model),
            model: run.model,
            chunks,
            embs,
        })
    }

    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    /// `(file, title)` of every indexed document, in index order.
    pub fn titles(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        for c in &self.chunks {
            if !out.iter().any(|(f, _)| *f == c.file) {
                out.push((c.file.clone(), c.title.clone()));
            }
        }
        out
    }

    pub fn chunk(&self, i: usize) -> &Chunk {
        &self.chunks[i]
    }

    fn embed_query(&self, question: &str) -> Res<Vec<f32>> {
        let mut q = self
            .embedder
            .embed(&[format!("{}{question}", rag::QUERY_PREFIX)])?
            .pop()
            .ok_or("ollama returned no query embedding")?;
        rag::normalize(&mut q);
        Ok(q)
    }

    /// Cosine of the query with every chunk, in index order; `None` for the
    /// skipped sections.
    pub fn cosines(&self, question: &str) -> Res<Vec<Option<f32>>> {
        let q = self.embed_query(question)?;
        Ok(self
            .chunks
            .iter()
            .zip(&self.embs)
            .map(|(c, e)| (!skipped(c)).then(|| rag::cosine(&q, e)))
            .collect())
    }
}

fn skipped(c: &Chunk) -> bool {
    SKIP_SECTIONS.iter().any(|s| c.section.starts_with(s))
}

/// What goes to the LLM instead of the bare question.
pub fn augment(question: &str, hits: &[Hit]) -> String {
    let mut s = String::from(
        "Answer the question using only the document excerpts below. If they do not contain the answer, \
         say that the documents do not contain it and stop: do not add an answer from your own knowledge. \
         Cite the excerpts you used as [1], [2], … Answer in the language of the question.\n\n<context>\n",
    );
    for (i, h) in hits.iter().enumerate() {
        s += &format!("[{}] {} — {}\n{}\n\n", i + 1, h.chunk.file, h.cite(), h.chunk.text.trim());
    }
    if hits.is_empty() {
        s += "(the search found no relevant excerpts)\n";
    }
    s += "</context>\n\nQuestion: ";
    s += question.trim();
    s
}

/// The transcript lines the chat shows above a RAG answer: the pipeline,
/// the rewritten query, every chunk with its scores, what the filter cut.
pub fn sources_note(r: &Retriever, p: &Pipeline, got: &Retrieval, added: usize) -> String {
    let mut lines = vec![format!(
        "RAG · {} · {} чанк(ов) из `{}` ({}), +{added} симв. к вопросу",
        p.label(),
        got.kept.len(),
        r.strategy,
        r.db.display()
    )];
    if let Some(q) = &got.rewritten {
        lines.push(format!("  запрос после rewrite: {q}"));
    }
    if !p.is_base() {
        lines.push(format!(
            "  кандидатов {} → порог similarity −{} → реранкер −{} → в контекст {}{}",
            got.pool.len(),
            got.dropped_sim,
            got.dropped_llm,
            got.kept.len(),
            if got.llm_calls > 0 {
                format!(" · {} выз. LLM, {} tok, {:.1} с", got.llm_calls, got.prompt_tokens + got.completion_tokens, got.ms as f64 / 1000.0)
            } else {
                String::new()
            }
        ));
    }
    for (i, h) in got.kept.iter().enumerate() {
        lines.push(format!("  [{}] {}", i + 1, hit_line(h)));
    }
    if got.kept.is_empty() {
        lines.push("  ничего не прошло фильтр — модель не вызывается: «не знаю» и просьба уточнить".into());
    }
    lines.extend(got.warnings.iter().map(|w| format!("  ! {w}")));
    lines.join("\n")
}

/// `#3 z 4.12 llm 9 · file — section, pages · 997 симв.`
pub fn hit_line(h: &Hit) -> String {
    format!(
        "#{} cos {:.4} z {:.2}{}{} · {} — {} · {} симв.",
        h.pos,
        h.score,
        h.z,
        h.llm.map(|s| format!(" llm {s}")).unwrap_or_default(),
        if h.from_rewrite { " (rewrite)" } else { "" },
        h.chunk.file,
        h.cite(),
        h.chunk.text.chars().count()
    )
}

/// One RAG turn's retrieval: the transcript note naming the chunks and
/// what the pipeline did; `cite::answer` builds the wire text from it.
pub struct Prepared {
    pub note: String,
    pub hits: Vec<Hit>,
    /// Characters the context added to the question.
    pub added: usize,
    pub retrieval: Retrieval,
    pub pipeline: Pipeline,
}

/// question → pipeline (rewrite / search / filter / rerank, as `settings`
/// say) → augmented question, with the default paths. What the chat runs
/// before a turn while `rag` is on, and `ask --rag` before its one.
pub fn prepare(question: &str, settings: &Settings) -> Res<Prepared> {
    prepare_with(question, settings, &Pipeline::from_settings(settings))
}

pub fn prepare_with(question: &str, settings: &Settings, p: &Pipeline) -> Res<Prepared> {
    let paths = default_paths();
    let r = Retriever::open(&paths.db, &settings.rag_strategy, &paths.url)?;
    let judge = if p.needs_llm() { Some(Judge::new(settings, &r)?) } else { None };
    let got = rerank::run(&r, p, judge.as_ref(), question)?;
    // task 24: what goes out is the grounded-answer prompt (`cite.rs`)
    let added = crate::cite::prompt(question, &got.kept, None).chars().count().saturating_sub(question.chars().count());
    Ok(Prepared { note: sources_note(&r, p, &got, added), hits: got.kept.clone(), added, retrieval: got, pipeline: p.clone() })
}

/// Where the chat and `--rag-eval` look for the index (`rag/index.sqlite`
/// next to `docs/`, resolved like `--rag-index`).
pub fn default_paths() -> rag::Config {
    rag::Config::locate(DEFAULT_DOCS, rag::DEFAULT_DB)
}

// ---------------------------------------------------------------- scoring

/// `alt|alt|…` — is any alternative in `text` at a word start
/// (case-insensitive)? Word starts keep `ares` from matching `shares`, and
/// prefixes let `hallucinat` match `hallucination(s)`.
pub fn covers(text: &str, group: &str) -> bool {
    let low = text.to_lowercase();
    group.split('|').map(|a| a.trim().to_lowercase()).filter(|a| !a.is_empty()).any(|alt| {
        low.match_indices(alt.as_str()).any(|(i, _)| {
            low[..i].chars().next_back().is_none_or(|c| !c.is_alphanumeric())
        })
    })
}

/// Groups of `must` found in `text`, and the ones missing.
pub fn coverage(text: &str, must: &[String]) -> (usize, Vec<String>) {
    let missing: Vec<String> = must.iter().filter(|g| !covers(text, g)).cloned().collect();
    (must.len() - missing.len(), missing)
}

/// Does `h` hold (part of) the answer of `c`: its file and a page listed
/// in `where` (`file:page`, or just `file`), or a section from `sources`?
pub fn expected(h: &Hit, c: &Control) -> bool {
    let ch = &h.chunk;
    let at = c.at.iter().any(|w| match w.rsplit_once(':') {
        Some((f, page)) => f == ch.file && page.trim().parse::<usize>().is_ok_and(|p| (ch.page_start..=ch.page_end).contains(&p)),
        None => *w == ch.file,
    });
    at || in_sources(ch.sections.iter().chain([&ch.section]), &c.sources)
}

/// Is `h` at least from a document that holds the answer? Chunks from other
/// documents are noise in the context.
pub fn expected_file(h: &Hit, c: &Control) -> bool {
    if c.at.is_empty() {
        return c.answerable() && !c.sources.is_empty();
    }
    c.at.iter().any(|w| w.rsplit_once(':').map_or(w.as_str(), |(f, _)| f) == h.chunk.file)
}

/// 1-based rank of the first expected hit (0 — none).
pub fn expected_rank(hits: &[Hit], c: &Control) -> usize {
    hits.iter().position(|h| expected(h, c)).map(|r| r + 1).unwrap_or(0)
}

fn in_sources<'a>(mut secs: impl Iterator<Item = &'a String>, sources: &[String]) -> bool {
    secs.any(|s| sources.iter().any(|want| s.to_lowercase().contains(&want.to_lowercase())))
}

/// Phrases of an answer that says the documents do not have it.
const REFUSAL: &str = "not contain|not mention|not include|not provide|not cover|not discuss|not address|not specify|\
                       not state|not appear|no information|no relevant|no mention|nothing relevant|no excerpt|\
                       cannot answer|can't answer|cannot find|unable to|not found|don't contain|doesn't contain|\
                       none of the|no answer|outside the scope|not in the";

pub fn refuses(text: &str) -> bool {
    covers(text, REFUSAL)
}

// ---------------------------------------------------------------- eval

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Control {
    pub id: usize,
    pub q: String,
    /// What a good answer says, in words — for the human reading the report.
    pub expect: String,
    /// The same, machine-checkable: every group must appear in the answer.
    /// Empty — the corpus has no answer, the model must say so.
    pub must: Vec<String>,
    /// Section(s) of the document that hold the answer (task 22's single
    /// IEEE paper; optional since task 23).
    #[serde(default)]
    pub sources: Vec<String>,
    /// `file:page` that hold the answer — checked against chunk metadata.
    #[serde(default, rename = "where")]
    pub at: Vec<String>,
    /// Unanswerable questions: groups that betray an answer made up from the
    /// model's own knowledge (`france` for the World Cup).
    #[serde(default)]
    pub wrong: Vec<String>,
}

impl Control {
    pub fn answerable(&self) -> bool {
        !self.must.is_empty()
    }

    /// `raft-consensus.pdf стр. 6, 15` / `нет в корпусе`
    pub fn where_label(&self) -> String {
        if !self.answerable() {
            return "нет в корпусе".into();
        }
        let mut files: Vec<(String, Vec<String>)> = Vec::new();
        for w in &self.at {
            let (f, p) = w.rsplit_once(':').map(|(f, p)| (f.to_string(), p.to_string())).unwrap_or((w.clone(), String::new()));
            match files.iter_mut().find(|(g, _)| *g == f) {
                Some((_, ps)) => ps.push(p),
                None => files.push((f, vec![p])),
            }
        }
        let mut parts: Vec<String> = files
            .into_iter()
            .map(|(f, ps)| {
                let ps: Vec<String> = ps.into_iter().filter(|p| !p.is_empty()).collect();
                if ps.is_empty() { f } else { format!("{f} стр. {}", ps.join(", ")) }
            })
            .collect();
        parts.extend(self.sources.iter().cloned());
        parts.join("; ")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// The model alone, no documents.
    Plain,
    /// Task 22's RAG: top-k by cosine (`base`).
    Rag,
    /// + similarity threshold.
    Sim,
    /// + LLM reranker with a threshold.
    Llm,
    /// + query rewrite, no filter.
    Rewrite,
    /// rewrite + similarity threshold + LLM reranker.
    Full,
    /// No index: the model reads the raw documents through `ask-docs-mcp`.
    Mcp,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Plain => "plain",
            Mode::Rag => "base",
            Mode::Sim => "sim",
            Mode::Llm => "llm",
            Mode::Rewrite => "rewrite",
            Mode::Full => "full",
            Mode::Mcp => "mcp",
        }
    }

    pub fn parse_list(s: &str) -> Res<Vec<Mode>> {
        s.split(',')
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(|m| match m {
                "plain" | "norag" | "off" => Ok(Mode::Plain),
                "base" | "rag" | "on" => Ok(Mode::Rag),
                "sim" | "filter" => Ok(Mode::Sim),
                "llm" | "rerank" => Ok(Mode::Llm),
                "rewrite" => Ok(Mode::Rewrite),
                "full" => Ok(Mode::Full),
                "mcp" | "raw" => Ok(Mode::Mcp),
                other => Err(format!("unknown eval mode {other:?}: plain | base | sim | llm | rewrite | full | mcp")),
            })
            .collect()
    }

    /// The retrieval pipeline of a RAG mode, thresholds and K from `tuned`.
    pub fn pipeline(self, tuned: &Pipeline) -> Option<Pipeline> {
        let (rewrite, filter) = match self {
            Mode::Rag => (false, Filter::Off),
            Mode::Sim => (false, Filter::Sim),
            Mode::Llm => (false, Filter::Llm),
            Mode::Rewrite => (true, Filter::Off),
            Mode::Full => (true, Filter::Both),
            Mode::Plain | Mode::Mcp => return None,
        };
        Some(Pipeline { rewrite, filter, ..tuned.clone() })
    }
}

pub struct EvalOpts {
    pub paths: rag::Config,
    pub strategy: String,
    pub modes: Vec<Mode>,
    /// Only these question ids (empty — all).
    pub only: Vec<usize>,
    /// K before / after and the thresholds every RAG mode shares.
    pub tuned: Pipeline,
}

/// What retrieval handed to one RAG answer.
#[derive(Debug, Serialize)]
pub struct Context {
    pub pipeline: String,
    pub rewritten: Option<String>,
    pub pool: usize,
    pub dropped_sim: usize,
    pub dropped_llm: usize,
    /// `hit_line` of every chunk that went to the model.
    pub kept: Vec<String>,
    /// 1-based rank of the first expected chunk among the kept / the pool.
    pub rank: usize,
    pub pool_rank: usize,
    /// Kept chunks from documents that do not hold the answer.
    pub noise: usize,
    /// Groups of `must` present in the kept text itself.
    pub covered: usize,
    pub chars: usize,
    pub llm_calls: usize,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub ms: u128,
    pub warnings: Vec<String>,
}

impl Context {
    fn of(p: &Pipeline, got: &Retrieval, c: &Control) -> Context {
        let text: String = got.kept.iter().map(|h| h.chunk.text.as_str()).collect::<Vec<_>>().join("\n");
        Context {
            pipeline: p.label(),
            rewritten: got.rewritten.clone(),
            pool: got.pool.len(),
            dropped_sim: got.dropped_sim,
            dropped_llm: got.dropped_llm,
            kept: got.kept.iter().map(hit_line).collect(),
            rank: expected_rank(&got.kept, c),
            pool_rank: expected_rank(&got.pool, c),
            noise: got.kept.iter().filter(|h| !expected_file(h, c)).count(),
            covered: coverage(&text, &c.must).0,
            chars: text.chars().count(),
            llm_calls: got.llm_calls,
            prompt_tokens: got.prompt_tokens,
            completion_tokens: got.completion_tokens,
            ms: got.ms,
            warnings: got.warnings.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Answer {
    pub mode: Mode,
    pub text: String,
    pub error: Option<String>,
    pub covered: usize,
    pub missing: Vec<String>,
    /// Says the documents have no answer.
    pub refused: bool,
    /// Unanswerable questions: `wrong` groups the answer contains anyway.
    pub made_up: Vec<String>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub latency_ms: u128,
    pub finish_reason: Option<String>,
    /// RAG modes: what retrieval handed over.
    pub context: Option<Context>,
    /// mcp: tool calls made and characters of document text they returned.
    pub tool_calls: usize,
    pub read_chars: usize,
    /// mcp: each call as `name {args} → N симв.`, in order.
    pub calls: Vec<String>,
    /// mcp: sections the model actually read (from `docs_read` results).
    pub read_sections: Vec<String>,
}

impl Answer {
    /// Unanswerable question handled honestly: refused, nothing made up.
    pub fn honest(&self) -> bool {
        self.error.is_none() && self.refused && self.made_up.is_empty()
    }

    pub fn total_ms(&self) -> u128 {
        self.latency_ms + self.context.as_ref().map(|c| c.ms).unwrap_or(0)
    }
}

#[derive(Debug, Serialize)]
pub struct Row {
    pub id: usize,
    pub q: String,
    pub expect: String,
    pub at: String,
    pub must: Vec<String>,
    pub answerable: bool,
    pub answers: Vec<Answer>,
}

const EVAL_SYSTEM: &str = "You are a helpful assistant. Answer in English, concisely: at most about 200 words.";

pub fn load_controls(dir: &Path) -> Res<Vec<Control>> {
    let path = dir.join(CONTROL_FILE);
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let set: Vec<Control> = serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
    if set.is_empty() {
        return Err(format!("{}: no questions", path.display()));
    }
    Ok(set)
}

/// The agent every mode shares: the user's model and sampling, but a fixed
/// system prompt and nothing else in `system` (no AGENTS.md, profile,
/// invariants, todo) — the only thing that differs between modes is how
/// the documents reach the model.
pub fn eval_agent(settings: &Settings) -> Res<Agent> {
    let mut s = settings.clone();
    s.system_prompt = EVAL_SYSTEM.into();
    s.context_enabled = false;
    s.invariants = false;
    s.todo = false;
    s.rag = false;
    s.profile = crate::profile::OFF.into();
    s.json_mode.enabled = false;
    s.max_chars = None;
    Agent::new(s)
}

/// One question through `agent`, scored against `c`; for an agent with
/// the docs toolbox it also records the calls and what they read.
pub fn answer(agent: &Agent, mode: Mode, prompt: &str, c: &Control) -> Answer {
    let t = Instant::now();
    let result = agent.complete_outcome(&[ChatMessage::user(prompt)]);
    let steps = agent
        .mcp()
        .and_then(|tb| tb.lock().ok().map(|mut tb| tb.take_log()))
        .unwrap_or_default();
    let mut read_sections: Vec<String> = Vec::new();
    for s in steps.iter().filter(|s| s.name == "docs_read" && !s.is_error) {
        for sec in s.structured["sections"].as_array().into_iter().flatten().filter_map(|v| v.as_str()) {
            if !read_sections.iter().any(|r| r == sec) {
                read_sections.push(sec.to_string());
            }
        }
    }
    let calls = steps
        .iter()
        .map(|s| format!("{}{} {} → {} симв.", if s.is_error { "✗ " } else { "" }, s.name, s.args, s.result.chars().count()))
        .collect();
    let read_chars = steps.iter().filter(|s| !s.is_error).map(|s| s.result.chars().count()).sum();
    let (text, error, usage, finish_reason, latency_ms) = match result {
        Ok(o) => (o.text().trim().to_string(), None, o.usage, o.finish_reason.clone(), o.latency_ms),
        Err(e) => (String::new(), Some(e), Default::default(), None, t.elapsed().as_millis()),
    };
    let (covered, missing) = coverage(&text, &c.must);
    let made_up = c.wrong.iter().filter(|g| covers(&text, g)).cloned().collect();
    Answer {
        mode,
        refused: refuses(&text),
        made_up,
        text,
        error,
        covered,
        missing,
        prompt_tokens: usage.prompt_tokens,
        completion_tokens: usage.completion_tokens,
        latency_ms,
        finish_reason,
        context: None,
        tool_calls: steps.len(),
        calls,
        read_chars,
        read_sections,
    }
}

pub fn eval(settings: &Settings, opts: &EvalOpts) -> Res<()> {
    let controls: Vec<Control> = load_controls(&opts.paths.dir)?
        .into_iter()
        .filter(|c| opts.only.is_empty() || opts.only.contains(&c.id))
        .collect();
    if controls.is_empty() {
        return Err("no control questions match --rag-eval-only".into());
    }
    let retriever = Retriever::open(&opts.paths.db, &opts.strategy, &opts.paths.url)?;
    let judge = Judge::new(settings, &retriever)?;
    let plain = eval_agent(settings)?;
    let mut mcp = eval_agent(settings)?;
    if opts.modes.contains(&Mode::Mcp) {
        let tb = crate::mcp_agent::Toolbox::local_docs(&opts.paths.dir)?;
        println!("MCP: {} — {}", tb.labels(), tb.tool_names());
        mcp.set_mcp(Some(std::sync::Arc::new(std::sync::Mutex::new(tb))));
    }
    println!(
        "модель {} · индекс {} ({}, {} чанков, {} документов) · режимы {} · вопросов {}",
        settings.model,
        retriever.db.display(),
        retriever.strategy,
        retriever.len(),
        retriever.titles().len(),
        opts.modes.iter().map(|m| m.name()).collect::<Vec<_>>().join(", "),
        controls.len()
    );
    for &m in &opts.modes {
        if let Some(p) = m.pipeline(&opts.tuned) {
            println!("  {:<8} {}", m.name(), p.label());
        }
    }
    let mut rows = Vec::new();
    for c in &controls {
        println!("\n[{}] {}\n    ответ: {}", c.id, c.q, c.where_label());
        let mut answers = Vec::new();
        for &mode in &opts.modes {
            let a = match mode.pipeline(&opts.tuned) {
                None if mode == Mode::Mcp => answer(&mcp, mode, &c.q, c),
                None => answer(&plain, mode, &c.q, c),
                Some(p) => match rerank::run(&retriever, &p, Some(&judge), &c.q) {
                    Ok(got) => {
                        let mut a = answer(&plain, mode, &augment(&c.q, &got.kept), c);
                        a.context = Some(Context::of(&p, &got, c));
                        a
                    }
                    Err(e) => return Err(format!("[{}] {}: {e}", c.id, mode.name())),
                },
            };
            println!("    {}", progress_line(&a, c));
            answers.push(a);
        }
        rows.push(Row {
            id: c.id,
            q: c.q.clone(),
            expect: c.expect.clone(),
            at: c.where_label(),
            must: c.must.clone(),
            answerable: c.answerable(),
            answers,
        });
    }
    let md = report(settings, &retriever, opts, &rows);
    let dir = opts.paths.db.parent().map(Path::to_path_buf).unwrap_or_default();
    let (md_path, json_path) = (dir.join("eval.md"), dir.join("eval.json"));
    std::fs::write(&md_path, &md).map_err(|e| format!("{}: {e}", md_path.display()))?;
    let raw = json!({"model": settings.model, "strategy": retriever.strategy, "tuned": opts.tuned, "rows": rows});
    std::fs::write(&json_path, serde_json::to_string_pretty(&raw).unwrap_or_default())
        .map_err(|e| format!("{}: {e}", json_path.display()))?;
    println!("\n{}", summary_table(&rows, &opts.modes));
    println!("отчёт: {} (+ {})", md_path.display(), json_path.display());
    Ok(())
}

fn progress_line(a: &Answer, c: &Control) -> String {
    let ctx = a
        .context
        .as_ref()
        .map(|x| {
            format!(
                " · контекст {} чанк., нужный на {}, чужих {}{}",
                x.kept.len(),
                if x.rank == 0 { "—".into() } else { x.rank.to_string() },
                x.noise,
                if x.llm_calls > 0 { format!(", этапы {} tok", x.prompt_tokens + x.completion_tokens) } else { String::new() }
            )
        })
        .unwrap_or_default();
    let verdict = if c.answerable() {
        format!("{}/{}", a.covered, c.must.len())
    } else if a.honest() {
        "честный отказ".into()
    } else if a.refused {
        format!("отказ, но ответил из своих знаний: {}", a.made_up.join(", "))
    } else {
        "ответил без опоры".into()
    };
    format!(
        "{:<8} {verdict} · prompt {} tok · {:.1} с{ctx}{}",
        a.mode.name(),
        a.prompt_tokens,
        a.total_ms() as f64 / 1000.0,
        match (&a.error, a.missing.is_empty()) {
            (Some(e), _) => format!(" · ОШИБКА: {e}"),
            (None, false) => format!(" · нет: {}", a.missing.join(", ")),
            _ => String::new(),
        }
    )
}

fn answers_of(rows: &[Row], mode: Mode) -> impl Iterator<Item = (&Row, &Answer)> {
    rows.iter().flat_map(move |r| r.answers.iter().filter(move |a| a.mode == mode).map(move |a| (r, a)))
}

pub fn summary_table(rows: &[Row], modes: &[Mode]) -> String {
    let groups: usize = rows.iter().map(|r| r.must.len()).sum();
    let answerable = rows.iter().filter(|r| r.answerable).count();
    let none = rows.len() - answerable;
    let mut md = format!(
        "| режим | покрытие ожиданий | ответов полностью (из {answerable}) | нужный чанк в контексте | чанков в контексте Σ | из чужих документов Σ | без ответа: пустой контекст (из {none}) | без ответа: честный отказ (из {none}) | ошибок | prompt tok ответа Σ | tok этапов Σ | время Σ |\n|---|---|---|---|---|---|---|---|---|---|---|---|\n"
    );
    for &m in modes {
        let all: Vec<(&Row, &Answer)> = answers_of(rows, m).collect();
        let ans: Vec<&(&Row, &Answer)> = all.iter().filter(|(r, _)| r.answerable).collect();
        let unans: Vec<&(&Row, &Answer)> = all.iter().filter(|(r, _)| !r.answerable).collect();
        let cov: usize = ans.iter().map(|(_, a)| a.covered).sum();
        let full = ans.iter().filter(|(r, a)| a.covered == r.must.len()).count();
        let errs = all.iter().filter(|(_, a)| a.error.is_some()).count();
        let ctx = |f: &dyn Fn(&Context) -> usize, set: &[&(&Row, &Answer)]| -> Option<usize> {
            let v: Vec<usize> = set.iter().filter_map(|(_, a)| a.context.as_ref().map(f)).collect();
            (!v.is_empty()).then(|| v.iter().sum())
        };
        let show = |v: Option<usize>| v.map(|v| v.to_string()).unwrap_or_else(|| "—".into());
        let hit = ctx(&|x| (x.rank > 0) as usize, &ans);
        let kept = ctx(&|x| x.kept.len(), &all.iter().collect::<Vec<_>>());
        let noise = ctx(&|x| x.noise, &all.iter().collect::<Vec<_>>());
        let empty = ctx(&|x| x.kept.is_empty() as usize, &unans);
        let honest = unans.iter().filter(|(_, a)| a.honest()).count();
        let stage: u64 = all.iter().filter_map(|(_, a)| a.context.as_ref()).map(|x| x.prompt_tokens + x.completion_tokens).sum();
        md += &format!(
            "| {} | {cov}/{groups} ({:.0}%) | {full}/{} | {} | {} | {} | {} | {honest}/{} | {errs} | {} | {} | {:.0} с |\n",
            m.name(),
            100.0 * cov as f64 / groups.max(1) as f64,
            ans.len(),
            hit.map(|h| format!("{h}/{}", ans.len())).unwrap_or_else(|| "—".into()),
            show(kept),
            show(noise),
            empty.map(|e| format!("{e}/{}", unans.len())).unwrap_or_else(|| "—".into()),
            unans.len(),
            all.iter().map(|(_, a)| a.prompt_tokens).sum::<u64>(),
            if stage > 0 { stage.to_string() } else { "—".into() },
            all.iter().map(|(_, a)| a.total_ms()).sum::<u128>() as f64 / 1000.0
        );
    }
    md
}

fn report(settings: &Settings, r: &Retriever, opts: &EvalOpts, rows: &[Row]) -> String {
    let mut md = format!(
        "# RAG: фильтрация, реранкинг и rewrite (задача 23)\n\n\
         Модель `{}`, индекс `{}` (стратегия `{}`, {} чанков из {} документов, эмбеддинги `{}`).\n\
         Вопросы и ожидания — `{}`: {} с ответом в корпусе и {} без ответа.\n\n\
         Общие параметры: top-K до фильтра = {}, top-K после = {}, порог similarity z ≥ {:.1}, порог реранкера ≥ {}.\n\n",
        settings.model,
        r.db.display(),
        r.strategy,
        r.len(),
        r.titles().len(),
        r.model,
        opts.paths.dir.join(CONTROL_FILE).display(),
        rows.iter().filter(|r| r.answerable).count(),
        rows.iter().filter(|r| !r.answerable).count(),
        opts.tuned.pool,
        opts.tuned.k,
        opts.tuned.min_z,
        opts.tuned.min_llm,
    );
    md += "| режим | что делает |\n|---|---|\n";
    for &m in &opts.modes {
        let what = match m.pipeline(&opts.tuned) {
            Some(p) => p.label(),
            None if m == Mode::Mcp => "индекса нет, модель сама читает PDF через ask-docs-mcp".into(),
            None => "модель без документов".into(),
        };
        md += &format!("| {} | {what} |\n", m.name());
    }
    md += "\n## Сводка\n\n";
    md += &summary_table(rows, &opts.modes);
    md += "\n«Нужный чанк в контексте» — среди отданных модели чанков есть чанк с нужного файла и страницы (`where` в control.json). \
           «Из чужих документов» — отданные чанки из файлов, где ответа нет (у вопросов без ответа — все). \
           «Честный отказ» — модель сказала, что в документах ответа нет, и не выдала ответ из своих знаний. \
           «tok этапов» — rewrite и реранкер (prompt + completion).\n";
    md += "\n## По вопросам\n\nЯчейка: покрытие ожидания · ранг нужного чанка в контексте / чанков в контексте / из них чужих.\n\n| # | вопрос | где ответ |";
    for m in &opts.modes {
        md += &format!(" {} |", m.name());
    }
    md += "\n|---|---|---|";
    md += &"---|".repeat(opts.modes.len());
    md += "\n";
    for row in rows {
        md += &format!("| {} | {} | {} |", row.id, row.q, row.at);
        for m in &opts.modes {
            let cell = row.answers.iter().find(|a| a.mode == *m).map(|a| cell(row, a)).unwrap_or_default();
            md += &format!(" {cell} |");
        }
        md += "\n";
    }
    md += "\n## Ответы\n";
    for row in rows {
        md += &format!(
            "\n### {}. {}\n\n**Ожидание:** {}\n\n**Где ответ:** {}{}\n",
            row.id,
            row.q,
            row.expect,
            row.at,
            if row.must.is_empty() { String::new() } else { format!(" · **проверяемые группы:** `{}`", row.must.join("`, `")) }
        );
        for a in &row.answers {
            let verdict = if row.answerable {
                format!(
                    "{}/{}{}",
                    a.covered,
                    row.must.len(),
                    if a.missing.is_empty() { String::new() } else { format!(", нет: `{}`", a.missing.join("`, `")) }
                )
            } else if a.honest() {
                "честный отказ".into()
            } else if a.refused {
                format!("отказ, но ответил из своих знаний: `{}`", a.made_up.join("`, `"))
            } else {
                "ответил без опоры на документы".into()
            };
            md += &format!("\n#### {} — {verdict}\n\n", a.mode.name());
            if let Some(x) = &a.context {
                md += &format!(
                    "_{} · кандидатов {} → −{} порогом similarity → −{} реранкером → {} в контексте ({} симв.){}_\n\n",
                    x.pipeline,
                    x.pool,
                    x.dropped_sim,
                    x.dropped_llm,
                    x.kept.len(),
                    x.chars,
                    if x.pool_rank > 0 { format!(" · нужный чанк среди кандидатов на {}", x.pool_rank) } else { String::new() }
                );
                if let Some(q) = &x.rewritten {
                    md += &format!("Запрос после rewrite: `{q}`\n\n");
                }
                for (i, k) in x.kept.iter().enumerate() {
                    md += &format!("{}. {k}\n", i + 1);
                }
                for w in &x.warnings {
                    md += &format!("- ! {w}\n");
                }
                md += "\n";
            }
            if a.mode == Mode::Mcp {
                md += &format!("_{} вызовов MCP, прочитано {} симв._\n\n", a.tool_calls, a.read_chars);
            }
            match &a.error {
                Some(e) => md += &format!("> ошибка: {e}\n"),
                None => {
                    for l in a.text.lines() {
                        md += &format!("> {l}\n");
                    }
                }
            }
        }
    }
    md
}

fn cell(row: &Row, a: &Answer) -> String {
    if a.error.is_some() {
        return "ошибка".into();
    }
    let verdict = if row.answerable {
        format!("{}/{}", a.covered, row.must.len())
    } else if a.honest() {
        "отказ ✓".into()
    } else if a.refused {
        "отказ + свои знания".into()
    } else {
        "ответ без опоры".into()
    };
    match &a.context {
        Some(x) => format!(
            "{verdict} · {}/{}/{}",
            if x.rank == 0 { "—".into() } else { x.rank.to_string() },
            x.kept.len(),
            x.noise
        ),
        None if a.mode == Mode::Mcp => format!("{verdict} · {} выз.", a.tool_calls),
        None => verdict,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(section: &str, text: &str) -> Chunk {
        Chunk {
            chunk_id: format!("structure-x-{section}"),
            strategy: "structure".into(),
            source: "docs/x.pdf".into(),
            file: "x.pdf".into(),
            title: "X".into(),
            section: section.into(),
            sections: vec![section.into()],
            page_start: 3,
            page_end: 4,
            char_start: 0,
            char_end: text.len(),
            text: text.into(),
        }
    }

    #[test]
    fn covers_matches_alternatives_at_word_starts() {
        assert!(covers("They HALLUCINATE often", "hallucinat"));
        assert!(covers("tools: RAGAS, ARES and TruLens", "ares"));
        assert!(!covers("the model shares its weights", "ares"));
        assert!(covers("accuracy rose by over 30%", "30"));
        assert!(!covers("in 2030", "30"));
        assert!(covers("use HyDE here", "step-back|hyde"));
        let must = vec!["naive".to_string(), "advanced".into(), "modular".into()];
        assert_eq!(coverage("Naive and Modular RAG", &must), (2, vec!["advanced".to_string()]));
    }

    #[test]
    fn references_are_never_searched() {
        assert!(skipped(&chunk("REFERENCES (part 1/9)", "refs")));
        assert!(!skipped(&chunk("A. Naive RAG", "naive")));
        let hits = vec![hit("A. Naive RAG", "naive"), hit("B. Advanced RAG", "adv")];
        let mut c = control(&["x"], &[]);
        c.sources = vec!["advanced rag".into()];
        assert_eq!(expected_rank(&hits, &c), 2);
        c.sources = vec!["Long Context".into()];
        assert_eq!(expected_rank(&hits, &c), 0);
    }

    #[test]
    fn augment_puts_cited_chunks_before_the_question() {
        let hits = vec![hit("C. Query Optimization", "HyDE builds hypothetical documents.")];
        let p = augment("What is HyDE?", &hits);
        let ctx = p.find("<context>").unwrap();
        let excerpt = p.find("[1] x.pdf — C. Query Optimization, стр. 3–4\nHyDE builds").unwrap();
        let q = p.find("Question: What is HyDE?").unwrap();
        assert!(ctx < excerpt && excerpt < q && p.ends_with("What is HyDE?"));
        let empty = augment("What is HyDE?", &[]);
        assert!(empty.contains("no relevant excerpts") && empty.contains("do not add an answer from your own knowledge"));
    }

    fn hit(section: &str, text: &str) -> Hit {
        Hit { chunk: chunk(section, text), score: 0.99, z: 0.0, llm: None, pos: 1, from_rewrite: false }
    }

    fn control(must: &[&str], at: &[&str]) -> Control {
        Control {
            id: 1,
            q: "q".into(),
            expect: String::new(),
            must: must.iter().map(|s| s.to_string()).collect(),
            sources: vec![],
            at: at.iter().map(|s| s.to_string()).collect(),
            wrong: vec![],
        }
    }

    #[test]
    fn expected_checks_file_and_page_range() {
        // chunk() spans pages 3–4 of x.pdf
        let h = hit("Front matter", "t");
        assert!(expected(&h, &control(&["a"], &["x.pdf:4"])));
        assert!(!expected(&h, &control(&["a"], &["x.pdf:5"])));
        assert!(!expected(&h, &control(&["a"], &["y.pdf:3"])));
        assert!(expected(&h, &control(&["a"], &["x.pdf"])));
        assert!(expected_file(&h, &control(&["a"], &["x.pdf:9"])));
        assert!(!expected_file(&h, &control(&["a"], &["y.pdf:3"])));
        // unanswerable: every chunk is noise
        assert!(!expected_file(&h, &control(&[], &[])));
        assert_eq!(control(&["a"], &["x.pdf:3", "x.pdf:9", "y.pdf"]).where_label(), "x.pdf стр. 3, 9; y.pdf");
    }

    #[test]
    fn refusals_are_recognised() {
        assert!(refuses("The provided excerpts do not contain information about Bitcoin."));
        assert!(refuses("There is no information about this in the documents."));
        assert!(!refuses("Raft uses election timeouts of 150–300 ms."));
    }

    #[test]
    fn control_set_spans_the_corpus_and_is_well_formed() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(DEFAULT_DOCS);
        let set = load_controls(&dir).unwrap();
        assert_eq!(set.len(), 26);
        let files: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|f| f.ends_with(".pdf"))
            .collect();
        assert_eq!(files.len(), 10);
        for f in &files {
            assert!(set.iter().any(|c| c.at.iter().any(|w| w.starts_with(f.as_str()))), "no question about {f}");
        }
        for (i, c) in set.iter().enumerate() {
            assert_eq!(c.id, i + 1);
            assert!(!c.expect.is_empty());
            if c.answerable() {
                assert!(!c.at.is_empty() && c.wrong.is_empty(), "question {}", c.id);
                for w in &c.at {
                    let (f, p) = w.rsplit_once(':').unwrap();
                    assert!(files.iter().any(|x| x == f) && p.parse::<usize>().is_ok(), "question {}: {w}", c.id);
                }
                // the expectation in words must itself satisfy the checkable groups
                assert_eq!(coverage(&c.expect, &c.must).1, Vec::<String>::new(), "question {}", c.id);
            } else {
                assert!(c.at.is_empty() && !c.wrong.is_empty(), "question {}", c.id);
            }
        }
        assert_eq!(set.iter().filter(|c| !c.answerable()).count(), 5);
    }

    #[test]
    fn modes_parse() {
        assert_eq!(Mode::parse_list("plain,rag, mcp").unwrap(), vec![Mode::Plain, Mode::Rag, Mode::Mcp]);
        assert_eq!(Mode::parse_list("base,sim,llm,rewrite,full").unwrap(), vec![Mode::Rag, Mode::Sim, Mode::Llm, Mode::Rewrite, Mode::Full]);
        assert!(Mode::parse_list("rag,x").is_err());
        let tuned = Pipeline { rewrite: false, filter: Filter::Off, pool: 20, k: 4, min_z: 3.0, min_llm: 5 };
        assert!(Mode::Rag.pipeline(&tuned).unwrap().is_base());
        let full = Mode::Full.pipeline(&tuned).unwrap();
        assert!(full.rewrite && full.filter == Filter::Both && full.pool == 20);
        assert!(Mode::Plain.pipeline(&tuned).is_none());
    }
}
