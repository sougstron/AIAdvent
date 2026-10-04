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

pub const DEFAULT_K: usize = 4;
pub const MAX_K: usize = 12;
pub const DEFAULT_STRATEGY: &str = "structure";
pub const DEFAULT_DOCS: &str = "docs";
pub const CONTROL_FILE: &str = "control.json";
/// The bibliography names every method of the survey and wins similarity
/// contests it holds no answer for (task 21's probe saw it outrank the
/// right section) — it is never handed to the model as context.
const SKIP_SECTIONS: [&str; 1] = ["REFERENCES"];

// ---------------------------------------------------------------- retrieval

#[derive(Clone, Debug)]
pub struct Hit {
    pub chunk: Chunk,
    pub score: f32,
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

    /// Title of the indexed document (the first chunk's), for the question
    /// header every eval mode gets.
    pub fn title(&self) -> &str {
        self.chunks.first().map(|c| c.title.as_str()).unwrap_or("")
    }

    pub fn search(&self, question: &str, k: usize) -> Res<Vec<Hit>> {
        let mut q = self
            .embedder
            .embed(&[format!("{}{question}", rag::QUERY_PREFIX)])?
            .pop()
            .ok_or("ollama returned no query embedding")?;
        rag::normalize(&mut q);
        Ok(rank(&q, &self.chunks, &self.embs, k))
    }
}

/// Top-`k` chunks by cosine, the skipped sections left out.
pub fn rank(q: &[f32], chunks: &[Chunk], embs: &[Vec<f32>], k: usize) -> Vec<Hit> {
    let mut scored: Vec<(f32, usize)> = embs
        .iter()
        .enumerate()
        .filter(|(i, _)| !SKIP_SECTIONS.iter().any(|s| chunks[*i].section.starts_with(s)))
        .map(|(i, e)| (rag::cosine(q, e), i))
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored
        .into_iter()
        .take(k)
        .map(|(score, i)| Hit { chunk: chunks[i].clone(), score })
        .collect()
}

/// What goes to the LLM instead of the bare question.
pub fn augment(question: &str, hits: &[Hit]) -> String {
    let mut s = String::from(
        "Answer the question using the document excerpts below. Rely on them first; if they do not \
         contain the answer, say so plainly instead of guessing. Cite the excerpts you used as [1], [2], … \
         Answer in the language of the question.\n\n<context>\n",
    );
    for (i, h) in hits.iter().enumerate() {
        s += &format!("[{}] {} — {}\n{}\n\n", i + 1, h.chunk.file, h.cite(), h.chunk.text.trim());
    }
    s += "</context>\n\nQuestion: ";
    s += question.trim();
    s
}

/// The transcript line the chat shows above a RAG answer.
pub fn sources_note(r: &Retriever, hits: &[Hit], added: usize) -> String {
    let mut lines = vec![format!(
        "RAG · {} чанка из `{}` ({}), +{added} симв. к вопросу:",
        hits.len(),
        r.strategy,
        r.db.display()
    )];
    for (i, h) in hits.iter().enumerate() {
        lines.push(format!(
            "  [{}] cos {:.4} · {} · {} симв.",
            i + 1,
            h.score,
            h.cite(),
            h.chunk.text.chars().count()
        ));
    }
    lines.join("\n")
}

/// One RAG turn ready for the wire: the augmented text, the transcript
/// note naming the chunks, and the hits themselves.
pub struct Prepared {
    pub wire: String,
    pub note: String,
    pub hits: Vec<Hit>,
    /// Characters the context added to the question.
    pub added: usize,
}

/// question → index → augmented question, with the default paths. What the
/// chat runs before a turn while `rag` is on, and `ask --rag` before its one.
pub fn prepare(question: &str, k: usize, strategy: &str) -> Res<Prepared> {
    let paths = default_paths();
    let r = Retriever::open(&paths.db, strategy, &paths.url)?;
    let hits = r.search(question, k)?;
    let wire = augment(question, &hits);
    let added = wire.chars().count().saturating_sub(question.chars().count());
    Ok(Prepared { note: sources_note(&r, &hits, added), wire, hits, added })
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

/// 1-based rank of the first hit inside an expected section (0 — none).
pub fn source_rank(hits: &[Hit], sources: &[String]) -> usize {
    hits.iter()
        .position(|h| in_sources(h.chunk.sections.iter().chain([&h.chunk.section]), sources))
        .map(|r| r + 1)
        .unwrap_or(0)
}

fn in_sources<'a>(mut secs: impl Iterator<Item = &'a String>, sources: &[String]) -> bool {
    secs.any(|s| sources.iter().any(|want| s.to_lowercase().contains(&want.to_lowercase())))
}

// ---------------------------------------------------------------- eval

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Control {
    pub id: usize,
    pub q: String,
    /// What a good answer says, in words — for the human reading the report.
    pub expect: String,
    /// The same, machine-checkable: every group must appear in the answer.
    pub must: Vec<String>,
    /// Section(s) of the document that hold the answer.
    pub sources: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Plain,
    Rag,
    Mcp,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Plain => "plain",
            Mode::Rag => "rag",
            Mode::Mcp => "mcp",
        }
    }

    pub fn parse_list(s: &str) -> Res<Vec<Mode>> {
        s.split(',')
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(|m| match m {
                "plain" | "norag" | "off" => Ok(Mode::Plain),
                "rag" | "on" => Ok(Mode::Rag),
                "mcp" | "raw" => Ok(Mode::Mcp),
                other => Err(format!("unknown eval mode {other:?}: plain | rag | mcp")),
            })
            .collect()
    }
}

pub struct EvalOpts {
    pub paths: rag::Config,
    pub k: usize,
    pub strategy: String,
    pub modes: Vec<Mode>,
    /// Only these question ids (empty — all).
    pub only: Vec<usize>,
}

#[derive(Debug, Serialize)]
pub struct Answer {
    pub mode: Mode,
    pub text: String,
    pub error: Option<String>,
    pub covered: usize,
    pub missing: Vec<String>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub latency_ms: u128,
    pub finish_reason: Option<String>,
    /// mcp: tool calls made and characters of document text they returned.
    pub tool_calls: usize,
    pub read_chars: usize,
    /// mcp: sections the model actually read (from `docs_read` results).
    pub read_sections: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Row {
    pub id: usize,
    pub q: String,
    pub expect: String,
    pub sources: Vec<String>,
    pub must: Vec<String>,
    /// `[n] section, pages · cos`
    pub retrieved: Vec<String>,
    pub source_rank: usize,
    /// Groups of `must` present in the retrieved text itself.
    pub context_covered: usize,
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
/// the document reaches the model.
fn eval_agent(settings: &Settings) -> Res<Agent> {
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

fn answer(agent: &Agent, mode: Mode, prompt: &str, must: &[String]) -> Answer {
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
    let read_chars = steps.iter().filter(|s| !s.is_error).map(|s| s.result.chars().count()).sum();
    let (text, error, usage, finish_reason, latency_ms) = match result {
        Ok(o) => (o.text().trim().to_string(), None, o.usage, o.finish_reason.clone(), o.latency_ms),
        Err(e) => (String::new(), Some(e), Default::default(), None, t.elapsed().as_millis()),
    };
    let (covered, missing) = coverage(&text, must);
    Answer {
        mode,
        text,
        error,
        covered,
        missing,
        prompt_tokens: usage.prompt_tokens,
        completion_tokens: usage.completion_tokens,
        latency_ms,
        finish_reason,
        tool_calls: steps.len(),
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
    let title = retriever.title().to_string();
    let plain = eval_agent(settings)?;
    let mut mcp = eval_agent(settings)?;
    if opts.modes.contains(&Mode::Mcp) {
        let tb = crate::mcp_agent::Toolbox::local_docs(&opts.paths.dir)?;
        println!("MCP: {} — {}", tb.labels(), tb.tool_names());
        mcp.set_mcp(Some(std::sync::Arc::new(std::sync::Mutex::new(tb))));
    }
    println!(
        "модель {} · индекс {} ({}, {} чанков, k={}) · режимы {} · вопросов {}",
        settings.model,
        retriever.db.display(),
        retriever.strategy,
        retriever.len(),
        opts.k,
        opts.modes.iter().map(|m| m.name()).collect::<Vec<_>>().join(", "),
        controls.len()
    );
    let mut rows = Vec::new();
    for c in &controls {
        // Every mode gets the same question and knows which document it is
        // about; only the way the document reaches the model differs.
        let question = format!("About the document “{title}”: {}", c.q);
        let hits = retriever.search(&c.q, opts.k)?;
        let rank = source_rank(&hits, &c.sources);
        let ctx: String = hits.iter().map(|h| h.chunk.text.as_str()).collect::<Vec<_>>().join("\n");
        let (ctx_cov, _) = coverage(&ctx, &c.must);
        println!(
            "\n[{}] {}\n    поиск: нужный раздел на ранге {} · в найденном тексте {}/{} ожидаемого",
            c.id,
            c.q,
            if rank == 0 { "—".to_string() } else { rank.to_string() },
            ctx_cov,
            c.must.len()
        );
        let mut answers = Vec::new();
        for &mode in &opts.modes {
            let a = match mode {
                Mode::Plain => answer(&plain, mode, &question, &c.must),
                Mode::Rag => answer(&plain, mode, &augment(&question, &hits), &c.must),
                Mode::Mcp => answer(&mcp, mode, &question, &c.must),
            };
            println!(
                "    {:<5} {}/{} ожидаемого · prompt {} tok · {:.1} с{}{}",
                mode.name(),
                a.covered,
                c.must.len(),
                a.prompt_tokens,
                a.latency_ms as f64 / 1000.0,
                if mode == Mode::Mcp { format!(" · {} вызовов, прочитано {} симв.", a.tool_calls, a.read_chars) } else { String::new() },
                match (&a.error, a.missing.is_empty()) {
                    (Some(e), _) => format!(" · ОШИБКА: {e}"),
                    (None, false) => format!(" · нет: {}", a.missing.join(", ")),
                    _ => String::new(),
                }
            );
            answers.push(a);
        }
        rows.push(Row {
            id: c.id,
            q: c.q.clone(),
            expect: c.expect.clone(),
            sources: c.sources.clone(),
            must: c.must.clone(),
            retrieved: hits.iter().map(|h| format!("{} · cos {:.4}", h.cite(), h.score)).collect(),
            source_rank: rank,
            context_covered: ctx_cov,
            answers,
        });
    }
    let md = report(settings, &retriever, opts, &rows);
    let dir = opts.paths.db.parent().map(Path::to_path_buf).unwrap_or_default();
    let (md_path, json_path) = (dir.join("eval.md"), dir.join("eval.json"));
    std::fs::write(&md_path, &md).map_err(|e| format!("{}: {e}", md_path.display()))?;
    let raw = json!({"model": settings.model, "strategy": retriever.strategy, "k": opts.k, "rows": rows});
    std::fs::write(&json_path, serde_json::to_string_pretty(&raw).unwrap_or_default())
        .map_err(|e| format!("{}: {e}", json_path.display()))?;
    println!("\n{}", summary_table(&rows, &opts.modes));
    println!("отчёт: {} (+ {})", md_path.display(), json_path.display());
    Ok(())
}

fn answers_of(rows: &[Row], mode: Mode) -> impl Iterator<Item = (&Row, &Answer)> {
    rows.iter().flat_map(move |r| r.answers.iter().filter(move |a| a.mode == mode).map(move |a| (r, a)))
}

pub fn summary_table(rows: &[Row], modes: &[Mode]) -> String {
    let groups: usize = rows.iter().map(|r| r.must.len()).sum();
    let mut md = String::from("| режим | покрытие ожиданий | вопросов полностью | ошибок | prompt tok (Σ) | completion tok (Σ) | время (Σ) |\n|---|---|---|---|---|---|---|\n");
    for &m in modes {
        let all: Vec<(&Row, &Answer)> = answers_of(rows, m).collect();
        let cov: usize = all.iter().map(|(_, a)| a.covered).sum();
        let full = all.iter().filter(|(r, a)| a.covered == r.must.len()).count();
        let errs = all.iter().filter(|(_, a)| a.error.is_some()).count();
        md += &format!(
            "| {} | {cov}/{groups} ({:.0}%) | {full}/{} | {errs} | {} | {} | {:.0} с |\n",
            m.name(),
            100.0 * cov as f64 / groups.max(1) as f64,
            all.len(),
            all.iter().map(|(_, a)| a.prompt_tokens).sum::<u64>(),
            all.iter().map(|(_, a)| a.completion_tokens).sum::<u64>(),
            all.iter().map(|(_, a)| a.latency_ms).sum::<u128>() as f64 / 1000.0
        );
    }
    md
}

fn report(settings: &Settings, r: &Retriever, opts: &EvalOpts, rows: &[Row]) -> String {
    let n = rows.len();
    let found = rows.iter().filter(|r| r.source_rank > 0).count();
    let top1 = rows.iter().filter(|r| r.source_rank == 1).count();
    let mut md = format!(
        "# RAG: контрольные вопросы (задача 22)\n\n\
         Модель `{}`, индекс `{}` (стратегия `{}`, {} чанков, эмбеддинги `{}`), k = {}.\n\
         Вопросы и ожидания — `{}`.\n\n\
         Поиск: нужный раздел среди {} найденных чанков — {found}/{n}, на первом месте — {top1}/{n}.\n\n## Сводка\n\n",
        settings.model,
        r.db.display(),
        r.strategy,
        r.len(),
        r.model,
        opts.k,
        opts.paths.dir.join(CONTROL_FILE).display(),
        opts.k,
    );
    md += &summary_table(rows, &opts.modes);
    md += "\n## По вопросам\n\nПокрытие — сколько групп ожидаемого (`must`) есть в ответе. «Контекст» — сколько их было в самих найденных чанках: если в контексте есть, а в ответе нет — промах генерации, если нет и в контексте — промах поиска.\n\n";
    md += "| # | вопрос | нужный раздел | ранг в поиске | контекст |";
    for m in &opts.modes {
        md += &format!(" {} |", m.name());
    }
    md += "\n|---|---|---|---|---|";
    md += &"---|".repeat(opts.modes.len());
    md += "\n";
    for row in rows {
        md += &format!(
            "| {} | {} | {} | {} | {}/{} |",
            row.id,
            row.q,
            row.sources.join("; "),
            if row.source_rank == 0 { "—".to_string() } else { row.source_rank.to_string() },
            row.context_covered,
            row.must.len()
        );
        for m in &opts.modes {
            let cell = row
                .answers
                .iter()
                .find(|a| a.mode == *m)
                .map(|a| {
                    let mut c = format!("{}/{}", a.covered, row.must.len());
                    if a.error.is_some() {
                        c += " (ошибка)";
                    } else if *m == Mode::Mcp {
                        let hit = in_sources(a.read_sections.iter(), &row.sources);
                        c += &format!(" · {} выз. · раздел {}", a.tool_calls, if hit { "прочитан" } else { "не читал" });
                    }
                    c
                })
                .unwrap_or_default();
            md += &format!(" {cell} |");
        }
        md += "\n";
    }
    md += "\n## Ответы\n";
    for row in rows {
        md += &format!(
            "\n### {}. {}\n\n**Ожидание:** {}\n\n**Источник:** {} · **проверяемые группы:** `{}`\n\n**Найдено RAG:**\n",
            row.id,
            row.q,
            row.expect,
            row.sources.join("; "),
            row.must.join("`, `")
        );
        for (i, h) in row.retrieved.iter().enumerate() {
            md += &format!("{}. {h}\n", i + 1);
        }
        for a in &row.answers {
            md += &format!(
                "\n#### {} — {}/{}{}\n\n",
                a.mode.name(),
                a.covered,
                row.must.len(),
                if a.missing.is_empty() { String::new() } else { format!(", нет: `{}`", a.missing.join("`, `")) }
            );
            if a.mode == Mode::Mcp {
                md += &format!(
                    "_{} вызовов MCP, прочитано {} симв., разделы: {}_\n\n",
                    a.tool_calls,
                    a.read_chars,
                    if a.read_sections.is_empty() { "—".into() } else { a.read_sections.join("; ") }
                );
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
    fn rank_skips_references_and_orders_by_cosine() {
        let chunks = vec![chunk("A. Naive RAG", "naive"), chunk("REFERENCES (part 1/9)", "refs"), chunk("B. Advanced RAG", "adv")];
        let embs = vec![vec![1.0, 0.0], vec![1.0, 0.01], vec![0.6, 0.8]];
        let hits = rank(&[1.0, 0.0], &chunks, &embs, 5);
        let secs: Vec<&str> = hits.iter().map(|h| h.chunk.section.as_str()).collect();
        assert_eq!(secs, ["A. Naive RAG", "B. Advanced RAG"]);
        assert!(hits[0].score > hits[1].score);
        assert_eq!(source_rank(&hits, &["advanced rag".into()]), 2);
        assert_eq!(source_rank(&hits, &["Long Context".into()]), 0);
    }

    #[test]
    fn augment_puts_cited_chunks_before_the_question() {
        let hits = vec![Hit { chunk: chunk("C. Query Optimization", "HyDE builds hypothetical documents."), score: 0.99 }];
        let p = augment("What is HyDE?", &hits);
        let ctx = p.find("<context>").unwrap();
        let excerpt = p.find("[1] x.pdf — C. Query Optimization, стр. 3–4\nHyDE builds").unwrap();
        let q = p.find("Question: What is HyDE?").unwrap();
        assert!(ctx < excerpt && excerpt < q && p.ends_with("What is HyDE?"));
    }

    #[test]
    fn control_set_is_ten_well_formed_questions() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(DEFAULT_DOCS);
        let set = load_controls(&dir).unwrap();
        assert_eq!(set.len(), 10);
        for (i, c) in set.iter().enumerate() {
            assert_eq!(c.id, i + 1);
            assert!(!c.must.is_empty() && !c.sources.is_empty() && !c.expect.is_empty());
            // the expectation in words must itself satisfy the checkable groups
            assert_eq!(coverage(&c.expect, &c.must).1, Vec::<String>::new(), "question {}", c.id);
        }
    }

    #[test]
    fn modes_parse() {
        assert_eq!(Mode::parse_list("plain,rag, mcp").unwrap(), vec![Mode::Plain, Mode::Rag, Mode::Mcp]);
        assert!(Mode::parse_list("rag,x").is_err());
    }
}
