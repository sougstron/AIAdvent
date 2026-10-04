//! Task 21: document indexing — the first step of RAG.
//!
//! Pipeline: documents (`.pdf` via `pdftotext`, `.md`, `.txt`) → text with
//! page and section offsets → chunks by one of two strategies → embeddings
//! from a local Ollama model → one SQLite index with metadata per chunk.
//!
//! * [`Strategy::Fixed`] — windows of `size` characters with `overlap`
//!   characters shared between neighbours, cut on whitespace;
//! * [`Strategy::Structure`] — one chunk per section (PDF numbered headings
//!   `III. RETRIEVAL` / `A. Retrieval Source`, Markdown `#`); a section longer
//!   than `struct_max` is split at sentence ends into `(part i/n)`.
//!
//! Every vector — document or query — is min-max normalized into `[0, 1]`
//! ([`normalize`]) before it is stored or compared.
//!
//! Every chunk carries `chunk_id`, `source`, `file`, `title`, `section`
//! (where it starts), `sections` (all it touches), pages and character
//! offsets. [`compare`] measures both strategies on the same text: sizes,
//! redundancy, how often a chunk crosses a section or ends mid-sentence, and
//! a retrieval probe — questions from `questions.json` whose answer section
//! is known, so hit@k is checked against metadata, not eyeballed.

use rusqlite::{params, Connection as Db};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::Res;

pub const DEFAULT_MODEL: &str = "nomic-embed-text";
pub const DEFAULT_URL: &str = "http://localhost:11434";
pub const DEFAULT_DB: &str = "rag/index.sqlite";
/// nomic-embed-text is trained with task prefixes; documents and queries
/// must use different ones or similarities drift.
pub const DOC_PREFIX: &str = "search_document: ";
pub const QUERY_PREFIX: &str = "search_query: ";
const BATCH: usize = 16;
const FRONT: &str = "Front matter";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    Fixed,
    Structure,
}

impl Strategy {
    pub fn name(self) -> &'static str {
        match self {
            Strategy::Fixed => "fixed",
            Strategy::Structure => "structure",
        }
    }

    pub fn parse_list(s: &str) -> Res<Vec<Strategy>> {
        match s {
            "fixed" => Ok(vec![Strategy::Fixed]),
            "structure" => Ok(vec![Strategy::Structure]),
            "both" => Ok(vec![Strategy::Fixed, Strategy::Structure]),
            other => Err(format!("unknown chunk strategy {other:?}: fixed | structure | both")),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub dir: PathBuf,
    pub strategies: Vec<Strategy>,
    /// Fixed strategy: window length, characters.
    pub size: usize,
    /// Fixed strategy: characters shared by neighbouring windows.
    pub overlap: usize,
    /// Structure strategy: longest section kept whole, characters.
    pub struct_max: usize,
    pub model: String,
    pub url: String,
    pub db: PathBuf,
}

impl Config {
    pub fn validate(&self) -> Res<()> {
        if self.size < 50 {
            return Err(format!("--chunk-size {} is too small (min 50)", self.size));
        }
        if self.overlap >= self.size {
            return Err(format!("--chunk-overlap {} must be smaller than --chunk-size {}", self.overlap, self.size));
        }
        if self.struct_max < 200 {
            return Err(format!("--struct-max {} is too small (min 200)", self.struct_max));
        }
        Ok(())
    }

    /// Relative `dir`/`db` are taken from the current directory; when `dir`
    /// is not there (e.g. launched from `target/release`), both fall back to
    /// the nearest ancestor of the executable that has `dir` (the task
    /// folder), and only then to the folder the binary was compiled in —
    /// that one is baked in at build time and may be another checkout.
    pub fn resolve_paths(mut self) -> Self {
        if !self.dir.is_relative() || self.dir.exists() {
            return self;
        }
        let exe = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok());
        let mut homes: Vec<PathBuf> = exe
            .iter()
            .flat_map(|p| p.ancestors().skip(1).map(Path::to_path_buf).collect::<Vec<_>>())
            .collect();
        homes.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        if let Some(home) = homes.into_iter().find(|h| h.join(&self.dir).exists()) {
            self.dir = home.join(&self.dir);
            if self.db.is_relative() {
                self.db = home.join(&self.db);
            }
        }
        self
    }

    fn params(&self, s: Strategy) -> Value {
        match s {
            Strategy::Fixed => json!({"size": self.size, "overlap": self.overlap, "norm": NORM}),
            Strategy::Structure => json!({"struct_max": self.struct_max, "norm": NORM}),
        }
    }
}

// ---------------------------------------------------------------- documents

#[derive(Clone, Debug)]
pub struct Section {
    pub path: String,
    /// Char offsets into [`Doc::chars`]; the heading line is inside.
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug)]
pub struct Doc {
    pub source: String,
    pub file: String,
    pub title: String,
    pub chars: Vec<char>,
    /// Char offset where each page starts (one entry for non-PDF files).
    pub pages: Vec<usize>,
    pub sections: Vec<Section>,
}

impl Doc {
    pub fn from_text(source: &str, text: &str, pages: Vec<usize>, markdown: bool) -> Doc {
        let chars: Vec<char> = text.chars().collect();
        let file = Path::new(source).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        let sections = split_sections(text, markdown);
        let title = guess_title(text, markdown).unwrap_or_else(|| file.clone());
        Doc { source: source.to_string(), file, title, chars, pages, sections }
    }

    fn page_at(&self, off: usize) -> usize {
        self.pages.iter().rposition(|&p| p <= off).map(|i| i + 1).unwrap_or(1)
    }

    fn sections_in(&self, start: usize, end: usize) -> Vec<String> {
        self.sections
            .iter()
            .filter(|s| s.start < end && start < s.end)
            .map(|s| s.path.clone())
            .collect()
    }
}

pub fn load_dir(dir: &Path) -> Res<Vec<Doc>> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| matches!(ext(p).as_str(), "pdf" | "md" | "txt"))
        .collect();
    paths.sort();
    if paths.is_empty() {
        return Err(format!("{}: no .pdf / .md / .txt documents", dir.display()));
    }
    paths.iter().map(|p| load_file(p)).collect()
}

fn ext(p: &Path) -> String {
    p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

pub fn load_file(p: &Path) -> Res<Doc> {
    let source = p.to_string_lossy().into_owned();
    match ext(p).as_str() {
        "pdf" => {
            let out = Command::new("pdftotext")
                .args(["-enc", "UTF-8"])
                .arg(p)
                .arg("-")
                .output()
                .map_err(|e| format!("pdftotext (poppler) is needed for {source}: {e}"))?;
            if !out.status.success() {
                return Err(format!("pdftotext {source}: {}", String::from_utf8_lossy(&out.stderr).trim()));
            }
            let raw = String::from_utf8_lossy(&out.stdout);
            let (text, pages) = join_pages(&raw);
            Ok(Doc::from_text(&source, &text, pages, false))
        }
        e => {
            let text = std::fs::read_to_string(p).map_err(|err| format!("{source}: {err}"))?;
            Ok(Doc::from_text(&source, &text, vec![0], e == "md"))
        }
    }
}

/// pdftotext separates pages with form feeds; keep the text, remember where
/// each page starts (in chars).
fn join_pages(raw: &str) -> (String, Vec<usize>) {
    let mut text = String::with_capacity(raw.len());
    let mut pages = Vec::new();
    let mut at = 0;
    for page in raw.split('\u{c}') {
        if page.trim().is_empty() {
            continue;
        }
        pages.push(at);
        text.push_str(page);
        if !page.ends_with('\n') {
            text.push('\n');
        }
        at = text.chars().count();
    }
    (text, pages)
}

fn guess_title(text: &str, markdown: bool) -> Option<String> {
    if markdown {
        return text.lines().find_map(|l| l.strip_prefix("# ").map(|t| t.trim().to_string()));
    }
    // pdftotext starts with the page number on some papers: skip it
    let blank_or_num = |l: &&str| l.trim().chars().all(|c| c.is_ascii_digit());
    let lines: Vec<&str> = text.lines().skip_while(blank_or_num).take_while(|l| !l.trim().is_empty()).take(2).collect();
    let t = lines.iter().map(|l| l.trim()).collect::<Vec<_>>().join(" ");
    (!t.is_empty() && t.chars().count() <= 160).then_some(t)
}

/// Section spans by headings. The text before the first heading is the
/// front matter. Paths are `LEVEL1 > LEVEL2`.
pub fn split_sections(text: &str, markdown: bool) -> Vec<Section> {
    let mut heads: Vec<(usize, String)> = Vec::new(); // (char offset, path)
    let mut stack: Vec<String> = Vec::new();
    let mut in_refs = false;
    let mut off = 0;
    for line in text.split_inclusive('\n') {
        let h = if markdown { md_heading(line) } else { pdf_heading(line, !stack.is_empty() && !in_refs) };
        if let Some((level, name)) = h {
            if level == 1 {
                in_refs = name.eq_ignore_ascii_case("REFERENCES");
            }
            stack.truncate(level - 1);
            while stack.len() < level - 1 {
                stack.push(String::new());
            }
            stack.push(name);
            let path = stack.iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join(" > ");
            heads.push((off, path));
        }
        off += line.chars().count();
    }
    let total = off;
    let mut out = Vec::new();
    let first = heads.first().map(|h| h.0).unwrap_or(total);
    if first > 0 {
        out.push(Section { path: FRONT.into(), start: 0, end: first });
    }
    for (i, (start, path)) in heads.iter().enumerate() {
        let end = heads.get(i + 1).map(|h| h.0).unwrap_or(total);
        out.push(Section { path: path.clone(), start: *start, end });
    }
    out
}

fn md_heading(line: &str) -> Option<(usize, String)> {
    let t = line.trim_end();
    let level = t.chars().take_while(|&c| c == '#').count();
    if !(1..=3).contains(&level) {
        return None;
    }
    let name = t[level..].strip_prefix(' ')?.trim();
    (!name.is_empty()).then(|| (level, name.to_string()))
}

/// IEEE-style headings as pdftotext prints them: `III. R ETRIEVAL` (small
/// caps split the first letter off), `A. Retrieval Source`, `R EFERENCES`.
/// Level-2 headings are only trusted inside a numbered section and outside
/// the references, where `A. Roberts` would otherwise look like one.
fn pdf_heading(line: &str, allow_sub: bool) -> Option<(usize, String)> {
    let t = line.trim();
    if t.is_empty() || t.chars().count() > 70 {
        return None;
    }
    let whole = join_small_caps(t);
    if matches!(whole.as_str(), "REFERENCES" | "ACKNOWLEDGMENT" | "ACKNOWLEDGMENTS" | "ACKNOWLEDGEMENT" | "APPENDIX") {
        return Some((1, whole));
    }
    let (num, rest) = t.split_once(". ")?;
    let rest = rest.trim();
    let letters = || rest.chars().filter(|c| c.is_alphabetic());
    if !num.is_empty() && num.len() <= 5 && num.chars().all(|c| "IVX".contains(c)) {
        let rest = join_small_caps(rest);
        if letters().count() >= 3 && rest.chars().filter(|c| c.is_alphabetic()).all(|c| c.is_uppercase()) {
            return Some((1, format!("{num}. {rest}")));
        }
        return None;
    }
    let mut n = num.chars();
    let (Some(c), None) = (n.next(), n.next()) else { return None };
    let ok = allow_sub
        && c.is_ascii_uppercase()
        && c <= 'L'
        && rest.chars().next().is_some_and(|f| f.is_uppercase())
        && rest.chars().count() <= 50
        && rest.split_whitespace().count() <= 7
        && !rest.contains(',')
        && !rest.ends_with('.')
        && letters().count() >= 3;
    ok.then(|| (2, format!("{c}. {rest}")))
}

/// `R EFERENCES` → `REFERENCES`, `TASK AND E VALUATION` → `TASK AND EVALUATION`.
fn join_small_caps(s: &str) -> String {
    let words: Vec<&str> = s.split_whitespace().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let w = words[i];
        let single = w.chars().count() == 1 && w.chars().all(|c| c.is_uppercase());
        let next_caps = words
            .get(i + 1)
            .is_some_and(|n| n.chars().count() >= 2 && n.chars().filter(|c| c.is_alphabetic()).all(|c| c.is_uppercase()));
        if single && next_caps {
            out.push(format!("{w}{}", words[i + 1]));
            i += 2;
        } else {
            out.push(w.to_string());
            i += 1;
        }
    }
    out.join(" ")
}

// ---------------------------------------------------------------- chunking

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Chunk {
    pub chunk_id: String,
    pub strategy: String,
    pub source: String,
    pub file: String,
    pub title: String,
    /// Section the chunk starts in.
    pub section: String,
    /// Every section the chunk touches (more than one → crosses a boundary).
    pub sections: Vec<String>,
    pub page_start: usize,
    pub page_end: usize,
    pub char_start: usize,
    pub char_end: usize,
    pub text: String,
}

impl Chunk {
    /// Fixed windows are labelled by where they start; when they run into
    /// further sections the label alone would hide that.
    fn label_is_span(&self) -> bool {
        self.strategy == Strategy::Fixed.name() && self.sections.len() > 1
    }
}

/// Spans `[start, end)` of about `size` chars, neighbours sharing about
/// `overlap` chars. Ends are pulled back to whitespace (never below half a
/// window) and starts pushed forward to a word start, so words stay whole.
pub fn fixed_spans(chars: &[char], size: usize, overlap: usize) -> Vec<(usize, usize)> {
    let n = chars.len();
    let mut out = Vec::new();
    let mut start = 0;
    while start < n && chars[start].is_whitespace() {
        start += 1;
    }
    while start < n {
        let mut end = (start + size).min(n);
        if end < n {
            let floor = start + size / 2;
            let mut e = end;
            while e > floor && !chars[e].is_whitespace() {
                e -= 1;
            }
            if e > floor {
                end = e;
            }
        }
        out.push((start, end));
        if end >= n {
            break;
        }
        let mut next = end.saturating_sub(overlap).max(start + 1);
        while next < end && !chars[next - 1].is_whitespace() {
            next += 1;
        }
        while next < n && chars[next].is_whitespace() {
            next += 1;
        }
        start = next;
    }
    out
}

/// Split `[start, end)` into the fewest pieces of at most `max` chars, of
/// about equal length (no 100-char tail after a 3900-char head), cutting at
/// the sentence end nearest the even split, else at whitespace.
pub fn sentence_spans(chars: &[char], start: usize, end: usize, max: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut s = start;
    while end - s > max {
        let left = (end - s).div_ceil(max);
        let ideal = s + (end - s) / left;
        let (floor, limit) = (s + max / 2, s + max);
        let near = |i: &usize| i.abs_diff(ideal);
        let cut = (floor..limit)
            .filter(|&i| matches!(chars[i], '.' | '?' | '!') && chars.get(i + 1).is_some_and(|c| c.is_whitespace()))
            .min_by_key(near)
            .map(|i| i + 1)
            .or_else(|| (floor..limit).filter(|&i| chars[i].is_whitespace()).min_by_key(near))
            .unwrap_or(limit);
        out.push((s, cut));
        s = cut;
    }
    out.push((s, end));
    out
}

pub fn chunk(doc: &Doc, strategy: Strategy, cfg: &Config) -> Vec<Chunk> {
    let stem = Path::new(&doc.file).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut spans: Vec<(usize, usize, Option<String>)> = Vec::new();
    match strategy {
        Strategy::Fixed => {
            spans.extend(fixed_spans(&doc.chars, cfg.size, cfg.overlap).into_iter().map(|(a, b)| (a, b, None)));
        }
        Strategy::Structure => {
            for sec in &doc.sections {
                let text: String = doc.chars[sec.start..sec.end].iter().collect();
                if text.trim().is_empty() {
                    continue;
                }
                let parts = sentence_spans(&doc.chars, sec.start, sec.end, cfg.struct_max);
                let n = parts.len();
                for (i, (a, b)) in parts.into_iter().enumerate() {
                    let label = if n > 1 { format!("{} (part {}/{n})", sec.path, i + 1) } else { sec.path.clone() };
                    spans.push((a, b, Some(label)));
                }
            }
        }
    }
    spans
        .into_iter()
        .filter_map(|(a, b, label)| {
            let text: String = doc.chars[a..b].iter().collect::<String>().trim().to_string();
            (!text.is_empty()).then_some((a, b, label, text))
        })
        .enumerate()
        .map(|(i, (a, b, label, text))| {
            let sections = doc.sections_in(a, b);
            let section = label.unwrap_or_else(|| sections.first().cloned().unwrap_or_else(|| FRONT.into()));
            Chunk {
                chunk_id: format!("{}-{stem}-{i:04}", strategy.name()),
                strategy: strategy.name().into(),
                source: doc.source.clone(),
                file: doc.file.clone(),
                title: doc.title.clone(),
                section,
                sections,
                page_start: doc.page_at(a),
                page_end: doc.page_at(b.saturating_sub(1).max(a)),
                char_start: a,
                char_end: b,
                text,
            }
        })
        .collect()
}

// ---------------------------------------------------------------- embeddings

pub struct Embedder {
    url: String,
    model: String,
    agent: ureq::Agent,
}

impl Embedder {
    pub fn new(url: &str, model: &str) -> Embedder {
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(300)).build();
        Embedder { url: url.trim_end_matches('/').to_string(), model: model.to_string(), agent }
    }

    /// One `/api/embed` call per batch; the count and dimension of what comes
    /// back are checked, not assumed.
    pub fn embed(&self, inputs: &[String]) -> Res<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(inputs.len());
        for batch in inputs.chunks(BATCH) {
            let body = json!({"model": self.model, "input": batch, "truncate": true});
            let resp: Value = self
                .agent
                .post(&format!("{}/api/embed", self.url))
                .send_json(body)
                .map_err(|e| match e {
                    ureq::Error::Status(code, r) => format!("ollama {code}: {}", r.into_string().unwrap_or_default()),
                    other => format!("ollama at {} unreachable ({other}); is `ollama serve` running?", self.url),
                })?
                .into_json()
                .map_err(|e| format!("ollama: bad JSON: {e}"))?;
            let embs = resp["embeddings"].as_array().ok_or_else(|| format!("ollama: no embeddings in {resp}"))?;
            if embs.len() != batch.len() {
                return Err(format!("ollama returned {} embeddings for {} inputs", embs.len(), batch.len()));
            }
            for e in embs {
                let v: Vec<f32> = e.as_array().into_iter().flatten().filter_map(|x| x.as_f64()).map(|x| x as f32).collect();
                if v.is_empty() || out.first().is_some_and(|f: &Vec<f32>| f.len() != v.len()) {
                    return Err(format!("ollama: inconsistent embedding dimension {}", v.len()));
                }
                out.push(v);
            }
        }
        Ok(out)
    }
}

/// Recorded in `runs.params`, so the index says how its vectors were scaled.
pub const NORM: &str = "minmax-0-1";

/// Min-max normalization of one vector into `[0, 1]`: `(x - min) / (max - min)`.
/// Plain division by the largest component is not enough — embedding
/// components are negative as often as positive, and those would stay below 0.
/// A constant vector carries no direction and becomes all zeros.
pub fn normalize(v: &mut [f32]) {
    let (lo, hi) = v.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), &x| (lo.min(x), hi.max(x)));
    let span = hi - lo;
    for x in v.iter_mut() {
        *x = if span > 0.0 { (*x - lo) / span } else { 0.0 };
    }
}

/// Smallest and largest component over a set of vectors.
pub fn range(vs: &[Vec<f32>]) -> (f32, f32) {
    vs.iter().flatten().fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), &x| (lo.min(x), hi.max(x)))
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot, mut na, mut nb) = (0f32, 0f32, 0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

// ---------------------------------------------------------------- storage

pub fn open(db: &Path) -> Res<Db> {
    if let Some(dir) = db.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let conn = Db::open(db).map_err(|e| format!("{}: {e}", db.display()))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS runs (
            strategy TEXT PRIMARY KEY, params TEXT NOT NULL, model TEXT NOT NULL,
            dim INTEGER NOT NULL, doc_prefix TEXT NOT NULL, query_prefix TEXT NOT NULL,
            docs INTEGER NOT NULL, source_chars INTEGER NOT NULL,
            embed_ms INTEGER NOT NULL, created INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS chunks (
            strategy TEXT NOT NULL, chunk_id TEXT NOT NULL, source TEXT NOT NULL,
            file TEXT NOT NULL, title TEXT NOT NULL, section TEXT NOT NULL,
            sections TEXT NOT NULL, page_start INTEGER NOT NULL, page_end INTEGER NOT NULL,
            char_start INTEGER NOT NULL, char_end INTEGER NOT NULL, text TEXT NOT NULL,
            embedding BLOB NOT NULL, PRIMARY KEY (strategy, chunk_id));",
    )
    .map_err(|e| format!("index schema: {e}"))?;
    Ok(conn)
}

fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn from_blob(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

pub struct Run {
    pub strategy: String,
    pub params: String,
    pub model: String,
    pub dim: usize,
    pub source_chars: usize,
    pub embed_ms: u64,
}

pub fn save(db: &mut Db, run: &Run, docs: usize, chunks: &[Chunk], embs: &[Vec<f32>]) -> Res<()> {
    let e = |e: rusqlite::Error| format!("index write: {e}");
    let tx = db.transaction().map_err(e)?;
    tx.execute("DELETE FROM chunks WHERE strategy = ?1", [&run.strategy]).map_err(e)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    tx.execute(
        "INSERT OR REPLACE INTO runs VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            run.strategy,
            run.params,
            run.model,
            run.dim as i64,
            DOC_PREFIX,
            QUERY_PREFIX,
            docs as i64,
            run.source_chars as i64,
            run.embed_ms as i64,
            now
        ],
    )
    .map_err(e)?;
    {
        let mut ins = tx.prepare("INSERT INTO chunks VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)").map_err(e)?;
        for (c, v) in chunks.iter().zip(embs) {
            ins.execute(params![
                c.strategy,
                c.chunk_id,
                c.source,
                c.file,
                c.title,
                c.section,
                serde_json::to_string(&c.sections).unwrap_or_default(),
                c.page_start as i64,
                c.page_end as i64,
                c.char_start as i64,
                c.char_end as i64,
                c.text,
                to_blob(v)
            ])
            .map_err(e)?;
        }
    }
    tx.commit().map_err(e)
}

/// One saved strategy: its run record, chunks and their vectors (same order).
pub type Saved = (Run, Vec<Chunk>, Vec<Vec<f32>>);

pub fn load_run(db: &Db, strategy: &str) -> Res<Option<Saved>> {
    let e = |e: rusqlite::Error| format!("index read: {e}");
    let run = db
        .query_row(
            "SELECT params, model, dim, source_chars, embed_ms FROM runs WHERE strategy = ?1",
            [strategy],
            |r| {
                Ok(Run {
                    strategy: strategy.to_string(),
                    params: r.get(0)?,
                    model: r.get(1)?,
                    dim: r.get::<_, i64>(2)? as usize,
                    source_chars: r.get::<_, i64>(3)? as usize,
                    embed_ms: r.get::<_, i64>(4)? as u64,
                })
            },
        )
        .ok();
    let Some(run) = run else { return Ok(None) };
    let mut st = db
        .prepare(
            "SELECT chunk_id, source, file, title, section, sections, page_start, page_end,
                    char_start, char_end, text, embedding
             FROM chunks WHERE strategy = ?1 ORDER BY chunk_id",
        )
        .map_err(e)?;
    let rows = st
        .query_map([strategy], |r| {
            let sections: String = r.get(5)?;
            let blob: Vec<u8> = r.get(11)?;
            Ok((
                Chunk {
                    chunk_id: r.get(0)?,
                    strategy: strategy.to_string(),
                    source: r.get(1)?,
                    file: r.get(2)?,
                    title: r.get(3)?,
                    section: r.get(4)?,
                    sections: serde_json::from_str(&sections).unwrap_or_default(),
                    page_start: r.get::<_, i64>(6)? as usize,
                    page_end: r.get::<_, i64>(7)? as usize,
                    char_start: r.get::<_, i64>(8)? as usize,
                    char_end: r.get::<_, i64>(9)? as usize,
                    text: r.get(10)?,
                },
                from_blob(&blob),
            ))
        })
        .map_err(e)?;
    let (mut chunks, mut embs) = (Vec::new(), Vec::new());
    for row in rows {
        let (c, v) = row.map_err(e)?;
        chunks.push(c);
        embs.push(v);
    }
    Ok(Some((run, chunks, embs)))
}

/// Metadata + text without vectors, one JSON per line — for reading the
/// chunks with a pager instead of SQL.
fn export_jsonl(path: &Path, chunks: &[Chunk]) -> Res<()> {
    let body: String = chunks.iter().map(|c| serde_json::to_string(c).unwrap_or_default() + "\n").collect();
    std::fs::write(path, body).map_err(|e| format!("{}: {e}", path.display()))
}

// ---------------------------------------------------------------- pipeline

pub fn index(cfg: &Config) -> Res<()> {
    cfg.validate()?;
    let docs = load_dir(&cfg.dir)?;
    let source_chars: usize = docs.iter().map(|d| d.chars.len()).sum();
    for d in &docs {
        println!(
            "документ {}: «{}», {} стр., {} символов, {} разделов",
            d.file,
            d.title,
            d.pages.len(),
            d.chars.len(),
            d.sections.len()
        );
    }
    let embedder = Embedder::new(&cfg.url, &cfg.model);
    let mut db = open(&cfg.db)?;
    for &s in &cfg.strategies {
        let chunks: Vec<Chunk> = docs.iter().flat_map(|d| chunk(d, s, cfg)).collect();
        println!("стратегия {} {}: {} чанков, эмбеддинги {} …", s.name(), cfg.params(s), chunks.len(), cfg.model);
        let inputs: Vec<String> = chunks.iter().map(|c| format!("{DOC_PREFIX}{}", c.text)).collect();
        let t = Instant::now();
        let mut embs = embedder.embed(&inputs)?;
        let embed_ms = t.elapsed().as_millis() as u64;
        let (raw_lo, raw_hi) = range(&embs);
        embs.iter_mut().for_each(|v| normalize(v));
        let (lo, hi) = range(&embs);
        println!("  нормализация min-max: компоненты [{raw_lo:.3}; {raw_hi:.3}] → [{lo:.3}; {hi:.3}]");
        let dim = embs.first().map(|v| v.len()).unwrap_or(0);
        let run = Run {
            strategy: s.name().into(),
            params: cfg.params(s).to_string(),
            model: cfg.model.clone(),
            dim,
            source_chars,
            embed_ms,
        };
        save(&mut db, &run, docs.len(), &chunks, &embs)?;
        let jsonl = cfg.db.with_file_name(format!("chunks-{}.jsonl", s.name()));
        export_jsonl(&jsonl, &chunks)?;
        println!("  сохранено: {} векторов dim={dim} за {embed_ms} мс → {} (+ {})", embs.len(), cfg.db.display(), jsonl.display());
    }
    compare(cfg)
}

// ---------------------------------------------------------------- comparison

#[derive(Debug, Deserialize)]
pub struct Question {
    pub q: String,
    /// Substring of the section path that holds the answer.
    pub expect: String,
}

#[derive(Debug, Default)]
pub struct Stats {
    pub chunks: usize,
    pub min: usize,
    pub median: usize,
    pub mean: usize,
    pub max: usize,
    pub tiny: usize,
    pub redundancy: f64,
    pub crossing: usize,
    pub mid_sentence: usize,
}

pub fn stats(chunks: &[Chunk], source_chars: usize) -> Stats {
    let mut lens: Vec<usize> = chunks.iter().map(|c| c.text.chars().count()).collect();
    lens.sort_unstable();
    if lens.is_empty() {
        return Stats::default();
    }
    let total: usize = lens.iter().sum();
    Stats {
        chunks: lens.len(),
        min: lens[0],
        median: lens[lens.len() / 2],
        mean: total / lens.len(),
        max: lens[lens.len() - 1],
        tiny: lens.iter().filter(|&&l| l < 200).count(),
        redundancy: total as f64 / source_chars.max(1) as f64,
        crossing: chunks.iter().filter(|c| c.sections.len() > 1).count(),
        mid_sentence: chunks.iter().filter(|c| !ends_sentence(&c.text)).count(),
    }
}

fn ends_sentence(t: &str) -> bool {
    t.trim_end().chars().last().is_some_and(|c| ".?!:;)]”\"".contains(c))
}

pub struct Probe {
    pub hit1: usize,
    /// Top-1 lies *wholly* in the expected section: a window that only
    /// grazes it with its tail counts for `hit1` but not here.
    pub pure1: usize,
    pub hit3: usize,
    pub mrr: f64,
    pub rows: Vec<(String, usize, f32, String)>, // (question, rank of first hit or 0, top-1 score, top-1 section)
}

/// A question is answered at rank k when the k-th nearest chunk touches the
/// expected section. Ranks are over chunks, so the fixed strategy is judged
/// by every section its window covers.
pub fn probe(questions: &[Question], qvecs: &[Vec<f32>], chunks: &[Chunk], embs: &[Vec<f32>]) -> Probe {
    let mut p = Probe { hit1: 0, pure1: 0, hit3: 0, mrr: 0.0, rows: Vec::new() };
    for (q, qv) in questions.iter().zip(qvecs) {
        let mut scored: Vec<(f32, usize)> = embs.iter().enumerate().map(|(i, e)| (cosine(qv, e), i)).collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        let want = q.expect.to_lowercase();
        let has = |s: &String| s.to_lowercase().contains(&want);
        let rank = scored
            .iter()
            .position(|&(_, i)| chunks[i].sections.iter().chain([&chunks[i].section]).any(has))
            .map(|r| r + 1)
            .unwrap_or(0);
        if rank == 1 {
            p.hit1 += 1;
            let top = &chunks[scored[0].1];
            if top.sections.iter().all(has) {
                p.pure1 += 1;
            }
        }
        if (1..=3).contains(&rank) {
            p.hit3 += 1;
        }
        if rank > 0 {
            p.mrr += 1.0 / rank as f64;
        }
        let (top, ti) = scored.first().copied().unwrap_or((0.0, 0));
        let top_sec = chunks
            .get(ti)
            .map(|c| match c.sections.as_slice() {
                [first, .., last] if c.label_is_span() => format!("{first} … {last}"),
                _ => c.section.clone(),
            })
            .unwrap_or_default();
        p.rows.push((q.q.clone(), rank, top, top_sec));
    }
    if !questions.is_empty() {
        p.mrr /= questions.len() as f64;
    }
    p
}

pub fn compare(cfg: &Config) -> Res<()> {
    let db = open(&cfg.db)?;
    let mut runs = Vec::new();
    for s in [Strategy::Fixed, Strategy::Structure] {
        if let Some(r) = load_run(&db, s.name())? {
            runs.push(r);
        }
    }
    if runs.is_empty() {
        return Err(format!("{}: index is empty, run --rag-index first", cfg.db.display()));
    }
    // Checked on the stored data, not on a flag: an index built before
    // normalization would be compared against normalized queries.
    for (r, _, embs) in &runs {
        let (lo, hi) = range(embs);
        if lo < 0.0 || hi > 1.0 {
            return Err(format!(
                "{}: strategy {} has components in [{lo:.3}; {hi:.3}], not normalized — run --rag-index again",
                cfg.db.display(),
                r.strategy
            ));
        }
    }
    let qpath = cfg.dir.join("questions.json");
    let questions: Vec<Question> = match std::fs::read_to_string(&qpath) {
        Ok(s) => serde_json::from_str(&s).map_err(|e| format!("{}: {e}", qpath.display()))?,
        Err(_) => Vec::new(),
    };
    let mut md = String::from("# Сравнение стратегий чанкинга\n\n");
    md += "| метрика | ";
    md += &runs.iter().map(|r| r.0.strategy.as_str()).collect::<Vec<_>>().join(" | ");
    md += " |\n|---|";
    md += &"---|".repeat(runs.len());
    md += "\n";
    let st: Vec<Stats> = runs.iter().map(|(r, c, _)| stats(c, r.source_chars)).collect();
    let pct = |a: usize, b: usize| format!("{a} ({:.0}%)", 100.0 * a as f64 / b.max(1) as f64);
    let mut row = |name: &str, f: &dyn Fn(usize) -> String| {
        md += &format!("| {name} | {} |\n", (0..runs.len()).map(f).collect::<Vec<_>>().join(" | "));
    };
    row("параметры", &|i| format!("`{}`", runs[i].0.params));
    row("модель / dim", &|i| format!("{} / {}", runs[i].0.model, runs[i].0.dim));
    row("чанков", &|i| st[i].chunks.to_string());
    row("символов min / медиана / среднее / max", &|i| {
        format!("{} / {} / {} / {}", st[i].min, st[i].median, st[i].mean, st[i].max)
    });
    row("мелких (< 200 симв.)", &|i| pct(st[i].tiny, st[i].chunks));
    row("избыточность (Σ чанков / текст)", &|i| format!("{:.2}×", st[i].redundancy));
    row("пересекают границу раздела", &|i| pct(st[i].crossing, st[i].chunks));
    row("обрываются посреди предложения", &|i| pct(st[i].mid_sentence, st[i].chunks));
    row("компоненты векторов min / max (нормализация min-max)", &|i| {
        let (lo, hi) = range(&runs[i].2);
        format!("{lo:.3} / {hi:.3}")
    });
    row("время эмбеддингов", &|i| {
        format!("{} мс ({:.0} мс/чанк)", runs[i].0.embed_ms, runs[i].0.embed_ms as f64 / st[i].chunks.max(1) as f64)
    });

    let mut probes = Vec::new();
    if !questions.is_empty() {
        let embedder = Embedder::new(&cfg.url, &runs[0].0.model);
        let inputs: Vec<String> = questions.iter().map(|q| format!("{QUERY_PREFIX}{}", q.q)).collect();
        let mut qvecs = embedder.embed(&inputs)?;
        qvecs.iter_mut().for_each(|v| normalize(v));
        for (_, chunks, embs) in &runs {
            probes.push(probe(&questions, &qvecs, chunks, embs));
        }
        let n = questions.len();
        let mut row = |name: &str, f: &dyn Fn(usize) -> String| {
            md += &format!("| {name} | {} |\n", (0..runs.len()).map(f).collect::<Vec<_>>().join(" | "));
        };
        row("поиск: hit@1 (чанк задевает раздел)", &|i| format!("{}/{n}", probes[i].hit1));
        row("поиск: hit@1 (чанк целиком в разделе)", &|i| format!("{}/{n}", probes[i].pure1));
        row("поиск: hit@3", &|i| format!("{}/{n}", probes[i].hit3));
        row("поиск: MRR", &|i| format!("{:.2}", probes[i].mrr));
        md += &format!("\n## Пробные вопросы ({})\n\nРанг — позиция первого чанка из ожидаемого раздела (0 — нет в выдаче).\n\n", qpath.display());
        md += "| вопрос | ожидаемый раздел | ";
        md += &runs.iter().map(|r| format!("{}: ранг / cos / top-1", r.0.strategy)).collect::<Vec<_>>().join(" | ");
        md += " |\n|---|---|";
        md += &"---|".repeat(runs.len());
        md += "\n";
        for (qi, q) in questions.iter().enumerate() {
            md += &format!("| {} | {} | ", q.q, q.expect);
            md += &probes
                .iter()
                .map(|p| {
                    let (_, rank, score, sec) = &p.rows[qi];
                    format!("{rank} / {score:.3} / {sec}")
                })
                .collect::<Vec<_>>()
                .join(" | ");
            md += " |\n";
        }
    }
    println!("\n{md}");
    let out = cfg.db.with_file_name("comparison.md");
    std::fs::write(&out, &md).map_err(|e| format!("{}: {e}", out.display()))?;
    println!("отчёт: {}", out.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(size: usize, overlap: usize, struct_max: usize) -> Config {
        Config {
            dir: PathBuf::from("docs"),
            strategies: vec![Strategy::Fixed, Strategy::Structure],
            size,
            overlap,
            struct_max,
            model: DEFAULT_MODEL.into(),
            url: DEFAULT_URL.into(),
            db: PathBuf::from(DEFAULT_DB),
        }
    }

    const PDF_LIKE: &str = "Retrieval-Augmented Generation for Large\nLanguage Models: A Survey\n\nAbstract—Large Language Models showcase things.\nI. I NTRODUCTION\nIntro text here. More intro.\nII. OVERVIEW OF RAG\nOverview words.\nA. Naive RAG\nNaive text, really naive.\nB. Advanced RAG\nAdvanced text.\nVI. TASK AND E VALUATION\nEval.\nR EFERENCES\n[1] N. Kandpal, H. Deng, “Large\nA. Roberts and others\n";

    #[test]
    fn pdf_headings_and_paths() {
        let secs = split_sections(PDF_LIKE, false);
        let paths: Vec<&str> = secs.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                FRONT,
                "I. INTRODUCTION",
                "II. OVERVIEW OF RAG",
                "II. OVERVIEW OF RAG > A. Naive RAG",
                "II. OVERVIEW OF RAG > B. Advanced RAG",
                "VI. TASK AND EVALUATION",
                "REFERENCES",
            ]
        );
        // spans tile the text exactly
        assert_eq!(secs[0].start, 0);
        for w in secs.windows(2) {
            assert_eq!(w[0].end, w[1].start);
        }
        assert_eq!(secs.last().unwrap().end, PDF_LIKE.chars().count());
    }

    #[test]
    fn markdown_headings() {
        let secs = split_sections("# Title\nintro\n## Setup\nx\n### Deep\ny\n## Use\nz\n", true);
        let paths: Vec<&str> = secs.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(paths, ["Title", "Title > Setup", "Title > Setup > Deep", "Title > Use"]);
        assert_eq!(guess_title("# Title\n", true).as_deref(), Some("Title"));
        assert_eq!(guess_title("1\n\nBig Paper\nTitle\n\nAuthors", false).as_deref(), Some("Big Paper Title"));
    }

    #[test]
    fn plain_sentences_are_not_headings() {
        assert!(pdf_heading("A. This is a sentence, with a comma", true).is_none());
        assert!(pdf_heading("A. Roberts", false).is_none());
        assert!(pdf_heading("I. am lowercase words", true).is_none());
        assert_eq!(pdf_heading("III. R ETRIEVAL", false), Some((1, "III. RETRIEVAL".into())));
    }

    #[test]
    fn fixed_spans_respect_size_overlap_and_words() {
        let text: String = (0..400).map(|i| format!("w{i} ")).collect();
        let chars: Vec<char> = text.chars().collect();
        let spans = fixed_spans(&chars, 100, 30);
        assert!(spans.len() > 10);
        assert_eq!(spans[0].0, 0);
        assert_eq!(spans.last().unwrap().1, chars.len());
        for w in spans.windows(2) {
            let ((a0, a1), (b0, _)) = (w[0], w[1]);
            assert!(a1 - a0 <= 100);
            let shared = a1.saturating_sub(b0);
            assert!((20..=30).contains(&shared), "overlap {shared}");
            assert!(chars[b0 - 1].is_whitespace() && !chars[b0].is_whitespace(), "starts on a word");
            assert!(a1 == chars.len() || chars[a1].is_whitespace(), "ends on a word");
        }
    }

    #[test]
    fn fixed_spans_without_overlap_tile_the_text() {
        let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa ".repeat(20);
        let chars: Vec<char> = text.chars().collect();
        let spans = fixed_spans(&chars, 60, 0);
        for w in spans.windows(2) {
            let between: String = chars[w[0].1..w[1].0].iter().collect();
            assert!(between.trim().is_empty());
        }
    }

    #[test]
    fn structure_chunks_follow_sections_and_split_long_ones() {
        let long = "One sentence here. ".repeat(40); // 760 chars
        let text = format!("# Doc\nintro\n## Short\nsmall.\n## Long\n{long}\n");
        let doc = Doc::from_text("docs/t.md", &text, vec![0], true);
        let chunks = chunk(&doc, Strategy::Structure, &cfg(100, 10, 300));
        assert_eq!(chunks[0].section, "Doc");
        assert_eq!(chunks[1].section, "Doc > Short");
        let parts: Vec<&Chunk> = chunks.iter().filter(|c| c.section.starts_with("Doc > Long")).collect();
        assert_eq!(parts.len(), 3);
        assert!(parts[0].section.ends_with("(part 1/3)"));
        for c in &chunks {
            assert!(c.text.chars().count() <= 300);
            assert_eq!(c.sections.len(), 1, "a structural chunk never crosses a section");
        }
        assert!(parts.iter().all(|c| c.text.ends_with('.')));
        let lens: Vec<usize> = parts.iter().map(|c| c.text.chars().count()).collect();
        assert!(lens.iter().max().unwrap() - lens.iter().min().unwrap() <= 40, "balanced parts {lens:?}");
        assert_eq!(chunks[2].chunk_id, "structure-t-0002");
    }

    #[test]
    fn fixed_chunks_carry_metadata_and_pages() {
        let doc = Doc::from_text("docs/x.pdf", PDF_LIKE, vec![0, 150], false);
        let chunks = chunk(&doc, Strategy::Fixed, &cfg(120, 20, 1000));
        assert!(chunks.iter().any(|c| c.sections.len() > 1), "fixed windows cross sections");
        assert_eq!(chunks[0].page_start, 1);
        assert_eq!(chunks.last().unwrap().page_end, 2);
        assert!(chunks.iter().all(|c| c.title.starts_with("Retrieval-Augmented") && c.file == "x.pdf"));
        assert_eq!(chunks[1].chunk_id, "fixed-x-0001");
    }

    #[test]
    fn join_pages_tracks_offsets() {
        let (text, pages) = join_pages("page one\n\u{c}page two\n\u{c}");
        assert_eq!(pages, vec![0, 9]);
        assert_eq!(&text[9..17], "page two");
    }

    #[test]
    fn normalize_maps_components_into_unit_range() {
        let mut v = vec![-0.5, 0.0, 1.5, 0.25];
        normalize(&mut v);
        assert_eq!(v, vec![0.0, 0.25, 1.0, 0.375]);
        // dividing by the max alone would leave -0.5 / 1.5 < 0
        let mut flat = vec![0.3; 4];
        normalize(&mut flat);
        assert_eq!(flat, vec![0.0; 4]);
        let mut vs = vec![vec![-2.0, 3.0, 0.5], vec![0.01, -0.02, 0.07]];
        vs.iter_mut().for_each(|v| normalize(v));
        assert_eq!(range(&vs), (0.0, 1.0));
        assert!(vs.iter().all(|v| v.contains(&0.0) && v.contains(&1.0)));
    }

    #[test]
    fn storage_roundtrip_and_probe() {
        let dir = std::env::temp_dir().join(format!("ask-rag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("i.sqlite");
        let mut db = open(&path).unwrap();
        let doc = Doc::from_text("docs/x.pdf", PDF_LIKE, vec![0], false);
        let chunks = chunk(&doc, Strategy::Structure, &cfg(100, 10, 1000));
        let embs: Vec<Vec<f32>> = (0..chunks.len()).map(|i| (0..4).map(|j| if i == j { 1.0 } else { 0.1 }).collect()).collect();
        let run = Run {
            strategy: "structure".into(),
            params: "{}".into(),
            model: "m".into(),
            dim: 4,
            source_chars: doc.chars.len(),
            embed_ms: 1,
        };
        save(&mut db, &run, 1, &chunks, &embs).unwrap();
        save(&mut db, &run, 1, &chunks, &embs).unwrap(); // re-index replaces, not duplicates
        let (r, c, e) = load_run(&db, "structure").unwrap().unwrap();
        assert_eq!((r.dim, c.len()), (4, chunks.len()));
        assert_eq!(e, embs);
        assert_eq!(c[3].sections, chunks[3].sections);
        // query vector closest to chunk 3 → its section ranks first
        let qs = vec![Question { q: "naive?".into(), expect: "naive rag".into() }];
        let p = probe(&qs, &[vec![0.1, 0.1, 0.1, 1.0]], &c, &e);
        assert_eq!(c[3].section, "II. OVERVIEW OF RAG > A. Naive RAG");
        assert_eq!((p.hit1, p.pure1, p.hit3), (1, 1, 1));
        // a window spanning two sections hits, but not purely
        let mut wide = c.clone();
        wide[3].sections.push("II. OVERVIEW OF RAG > B. Advanced RAG".into());
        let p = probe(&qs, &[vec![0.1, 0.1, 0.1, 1.0]], &wide, &e);
        assert_eq!((p.hit1, p.pure1), (1, 0));
        assert!(load_run(&db, "fixed").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
