//! Task 19: composition of MCP tools. A third own MCP server,
//! `ask-pipeline-mcp`, with three tools that make one pipeline:
//!
//! * `search`     — gets the data: lines of text files under `root` that
//!   contain every word of the query, or Wikipedia article intros;
//! * `summarize`  — processes it: a deterministic extractive summary (the
//!   highest-scoring sentences by word frequency, in original order);
//! * `saveToFile` — stores the result as a file in `out`.
//!
//! Every result gets an id (`r1`, `r2`, …) and a digest of its data. The
//! next tool takes the data either by value (`text` / `content`) or by
//! reference (`input_id`), and echoes the digest of what it actually
//! received — so a hand-off between tools is checked by comparing digests,
//! not by eyeballing text.
//!
//! [`run_chain`] is the automatic pipeline (search → summarize →
//! saveToFile, every step a real `tools/call` over HTTP, data carried by the
//! client), [`audit`] checks the hand-offs of a chain the *model* assembled
//! in the chat, and [`verify`] is the causal proof (`ask --verify-pipeline`).

use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::api::Endpoint;
use crate::config::{Res, Settings};
use crate::mcp::{CallResult, Connection};
use crate::mcp_agent::{self, ToolCaller, ToolStep, Toolbox};
use crate::mcp_server::{self, CallLog, ServerInfo};

pub const DEFAULT_PORT: u16 = 8767;
pub const SERVER_NAME: &str = "ask-pipeline-mcp";
pub const TOOLS: [&str; 3] = ["search", "summarize", "saveToFile"];
const MAX_FILE_BYTES: u64 = 512 * 1024;
const MAX_HIT_CHARS: usize = 200;
const MAX_WIKI_CHARS: usize = 1500;

/// FNV-1a 64 of the data, hex. Not cryptographic — it only has to tell
/// "the same bytes" from "different bytes" in a hand-off.
pub fn digest(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

pub fn short(d: &str) -> &str {
    &d[..d.len().min(8)]
}

/// A tool result is one header line (`[r1] search …`) and the data. The
/// header is for the reader (and tells the model the id); only the data
/// travels on and is digested.
pub fn split_header(text: &str) -> (Option<&str>, &str) {
    if text.starts_with("[r") {
        if let Some((head, body)) = text.split_once('\n') {
            return (Some(head), body);
        }
        return (Some(text), "");
    }
    (None, text)
}

/// Default output folder for saved files: `~/.ask6/pipeline/`.
pub fn default_out() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(".ask6/pipeline")
}

// ---------------------------------------------------------------- server

struct Stored {
    id: String,
    tool: &'static str,
    data: String,
}

#[derive(Clone)]
pub struct Server {
    root: PathBuf,
    out: PathBuf,
    results: Arc<Mutex<Vec<Stored>>>,
    pub calls: CallLog,
    verbose: bool,
}

impl Server {
    pub fn new(root: &Path, out: &Path, verbose: bool) -> Res<Server> {
        let root = root
            .canonicalize()
            .map_err(|e| format!("search root {}: {e}", root.display()))?;
        std::fs::create_dir_all(out).map_err(|e| format!("output dir {}: {e}", out.display()))?;
        let out = out.canonicalize().map_err(|e| e.to_string())?;
        Ok(Server {
            root,
            out,
            results: Arc::default(),
            calls: Arc::default(),
            verbose,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn out(&self) -> &Path {
        &self.out
    }

    pub fn serve(&self, listener: TcpListener) {
        for stream in listener.incoming().flatten() {
            let r = mcp_server::handle_connection(stream, "mcp-pipeline", self.verbose, &|m| self.handle(m));
            if let (Err(e), true) = (r, self.verbose) {
                eprintln!("[mcp-pipeline] connection error: {e}");
            }
        }
    }

    pub fn spawn(&self, port: u16) -> Res<String> {
        let listener = TcpListener::bind(("127.0.0.1", port))
            .map_err(|e| format!("bind 127.0.0.1:{port}: {e}"))?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        let server = self.clone();
        std::thread::spawn(move || server.serve(listener));
        Ok(format!("http://{addr}/mcp"))
    }

    pub fn handle(&self, msg: &Value) -> Option<Value> {
        let info = ServerInfo {
            name: SERVER_NAME,
            instructions: format!(
                "A data pipeline of three tools: `search` gets data (text files under {} or \
                 Wikipedia), `summarize` processes it, `saveToFile` stores the result in {}. When \
                 the user asks to find something and summarize and/or save it, call them in this \
                 order and hand each result to the next tool by `input_id` (the `[rN]` at the start \
                 of the previous result) instead of retyping the text. Only results of these three \
                 tools have ids; output of other MCP tools (e.g. git_log) goes in as `text` / \
                 `content`, never together with `input_id`.",
                self.root.display(),
                self.out.display()
            ),
        };
        mcp_server::dispatch(msg, &info, &tool_specs(), &self.calls, &|name, args| self.call(name, args))
    }

    fn call(&self, name: &str, args: &Value) -> Res<(String, Value)> {
        match name {
            "search" => self.search(args),
            "summarize" => self.summarize(args),
            "saveToFile" => self.save(args),
            _ => Err(format!("unknown tool: {name}")),
        }
    }

    fn store(&self, tool: &'static str, data: &str) -> Res<String> {
        let mut results = self.results.lock().map_err(|e| e.to_string())?;
        let id = format!("r{}", results.len() + 1);
        results.push(Stored { id: id.clone(), tool, data: data.to_string() });
        Ok(id)
    }

    /// The data a tool works on: `input_id` (a stored result) or the value
    /// under `key`, exactly one of them. Returns the data and a description
    /// of where it came from, digest included.
    fn input(&self, args: &Value, key: &str) -> Res<(String, Value)> {
        let by_ref = args["input_id"].as_str().filter(|s| !s.trim().is_empty());
        let by_value = args[key].as_str().filter(|s| !s.trim().is_empty());
        let (data, via, from) = match (by_ref, by_value) {
            (Some(_), Some(_)) => {
                return Err(format!(
                    "give either `input_id` or `{key}`, not both: ids (r1, r2 …) exist only for results \
                     of search / summarize / saveToFile, other tools' output goes in `{key}` alone"
                ))
            }
            (None, None) => return Err(format!("`input_id` or `{key}` is required")),
            (Some(id), None) => {
                let results = self.results.lock().map_err(|e| e.to_string())?;
                let s = results
                    .iter()
                    .find(|r| r.id == id.trim())
                    .ok_or_else(|| format!("no result with id `{id}` (ids look like r1, r2 …)"))?;
                (s.data.clone(), "input_id", json!(format!("{} {}", s.tool, s.id)))
            }
            // A header copied along with the text is not data.
            (None, Some(text)) => (split_header(text).1.to_string(), key, Value::Null),
        };
        let d = digest(&data);
        Ok((data.clone(), json!({"via": via, "from": from, "chars": data.chars().count(), "digest": d})))
    }

    fn search(&self, args: &Value) -> Res<(String, Value)> {
        let query = args["query"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or("`query` (string) is required")?;
        let source = args["source"].as_str().unwrap_or("files");
        let (hits, truncated) = match source {
            "files" => {
                let limit = limit_arg(args, 20, 50)?;
                let exact = args["case_sensitive"].as_bool().unwrap_or(false);
                let start = self.scope(args)?;
                search_files(&self.root, &start, &self.out, query, limit, exact)?
            }
            "wikipedia" => {
                let limit = limit_arg(args, 3, 10)?;
                let lang = args["lang"].as_str().unwrap_or("ru");
                if !lang.chars().all(|c| c.is_ascii_lowercase()) || lang.is_empty() || lang.len() > 3 {
                    return Err("`lang` must be a Wikipedia language code like ru or en".into());
                }
                (search_wikipedia(query, lang, limit)?, false)
            }
            other => return Err(format!("`source` must be \"files\" or \"wikipedia\", not {other:?}")),
        };
        let data = hits
            .iter()
            .map(|h| match source {
                "files" => format!("{}: {}", h.0, h.1),
                _ => format!("## {}\n{}", h.0, h.1),
            })
            .collect::<Vec<_>>()
            .join("\n");
        let id = self.store("search", &data)?;
        let d = digest(&data);
        let head = format!(
            "[{id}] search «{query}» ({source}): {} {}, {} симв, #{}{}",
            hits.len(),
            match source {
                "files" => plural(hits.len(), "совпадение", "совпадения", "совпадений"),
                _ => plural(hits.len(), "статья", "статьи", "статей"),
            },
            data.chars().count(),
            short(&d),
            if truncated { ", обрезано по limit" } else { "" }
        );
        let text = if hits.is_empty() {
            format!("{head}\n(ничего не найдено)")
        } else {
            format!("{head}\n{data}")
        };
        let hits: Vec<Value> = hits.iter().map(|(w, t)| json!({"where": w, "text": t})).collect();
        Ok((
            text,
            json!({"id": id, "tool": "search", "query": query, "source": source, "count": hits.len(),
                   "truncated": truncated, "hits": hits, "chars": data.chars().count(), "digest": d}),
        ))
    }

    /// Task 20: `path` narrows a files search to one folder or file under the
    /// root (the repository), e.g. the code of one task instead of every
    /// snapshot; hits stay relative to the root so `git_log{path}` takes them
    /// as they are. Nothing outside the root is reachable.
    fn scope(&self, args: &Value) -> Res<PathBuf> {
        let Some(p) = args["path"].as_str().map(str::trim).filter(|p| !p.is_empty() && *p != ".") else {
            return Ok(self.root.clone());
        };
        let full = self
            .root
            .join(p.trim_start_matches("./"))
            .canonicalize()
            .map_err(|_| format!("`path` {p:?}: no such folder or file under {}", self.root.display()))?;
        if !full.starts_with(&self.root) {
            return Err(format!("`path` {p:?} is outside {}", self.root.display()));
        }
        Ok(full)
    }

    fn summarize(&self, args: &Value) -> Res<(String, Value)> {
        let (data, input) = self.input(args, "text")?;
        let max = limit_arg_named(args, "max_sentences", 5, 20)?;
        let s = summarize_text(&data, max);
        if s.text.is_empty() {
            return Err("nothing to summarize: the input has no sentences".into());
        }
        let id = self.store("summarize", &s.text)?;
        let d = digest(&s.text);
        let head = format!(
            "[{id}] summarize: {} из {} предложений, {} симв, #{} (вход #{})",
            s.picked,
            s.total,
            s.text.chars().count(),
            short(&d),
            short(input["digest"].as_str().unwrap_or(""))
        );
        Ok((
            format!("{head}\n{}", s.text),
            json!({"id": id, "tool": "summarize", "input": input, "sentences_in": s.total,
                   "sentences_out": s.picked, "keywords": s.keywords,
                   "chars": s.text.chars().count(), "digest": d}),
        ))
    }

    fn save(&self, args: &Value) -> Res<(String, Value)> {
        let name = args["filename"]
            .as_str()
            .map(str::trim)
            .ok_or("`filename` (string) is required")?;
        check_filename(name)?;
        let (data, input) = self.input(args, "content")?;
        let path = self.out.join(name);
        std::fs::write(&path, &data).map_err(|e| format!("write {}: {e}", path.display()))?;
        // Digest of what is on disk now, read back — not of what we meant to write.
        let back = std::fs::read_to_string(&path).map_err(|e| format!("read back {}: {e}", path.display()))?;
        let d = digest(&back);
        let id = self.store("saveToFile", &back)?;
        let head = format!(
            "[{id}] saveToFile: {} байт → {}, #{}",
            back.len(),
            path.display(),
            short(&d)
        );
        Ok((
            head,
            json!({"id": id, "tool": "saveToFile", "input": input, "path": path.display().to_string(),
                   "bytes": back.len(), "digest": d}),
        ))
    }
}

fn limit_arg(args: &Value, default: u64, max: u64) -> Res<usize> {
    limit_arg_named(args, "limit", default, max)
}

fn limit_arg_named(args: &Value, key: &str, default: u64, max: u64) -> Res<usize> {
    match &args[key] {
        Value::Null => Ok(default as usize),
        v => v
            .as_u64()
            .filter(|n| (1..=max).contains(n))
            .map(|n| n as usize)
            .ok_or_else(|| format!("`{key}` must be an integer from 1 to {max}")),
    }
}

/// A plain file name inside the output folder: no paths, no dot-files.
fn check_filename(name: &str) -> Res<()> {
    let ok = !name.is_empty()
        && name.len() <= 80
        && !name.starts_with('.')
        && name.chars().all(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "`filename` must be a plain name like notes.md (letters, digits, . - _), got {name:?}"
        ))
    }
}

/// What `tools/list` returns.
pub fn tool_specs() -> Vec<Value> {
    vec![
        json!({
            "name": "search",
            "description": "Step 1 of the pipeline — get data. Searches text files under the server's root folder (every line that contains all words of the query, as `path:line: text`) or Wikipedia (article intros). The first line of the result is `[rN] …`: pass rN as `input_id` to the next tool.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Words to look for."},
                    "source": {"type": "string", "enum": ["files", "wikipedia"], "description": "Where to search (default files)."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 50, "description": "Max hits: files default 20 (≤50), wikipedia default 3 (≤10)."},
                    "lang": {"type": "string", "description": "Wikipedia language code (default ru)."},
                    "case_sensitive": {"type": "boolean", "description": "files only: match the words case-sensitively (default false), e.g. to find `TODO:` markers but not the word todo."},
                    "path": {"type": "string", "description": "files only: search just this folder or file, relative to the root (e.g. tree/task-20/src). Hits keep paths relative to the root."},
                },
                "required": ["query"],
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "summarize",
            "description": "Step 2 of the pipeline — process data. Deterministic extractive summary: keywords plus the most informative sentences in their original order. Takes a previous result by `input_id` (preferred) or raw `text`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "input_id": {"type": "string", "description": "Id of a previous result, e.g. r1."},
                    "text": {"type": "string", "description": "Text to summarize, if not by input_id."},
                    "max_sentences": {"type": "integer", "minimum": 1, "maximum": 20, "description": "How many sentences to keep (default 5)."},
                },
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "saveToFile",
            "description": "Step 3 of the pipeline — store the result as a file in the server's output folder and return its path. Takes a previous result by `input_id` (preferred) or raw `content`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "filename": {"type": "string", "description": "Plain file name like notes.md (no folders)."},
                    "input_id": {"type": "string", "description": "Id of a previous result, e.g. r2."},
                    "content": {"type": "string", "description": "Text to write, if not by input_id."},
                },
                "required": ["filename"],
                "additionalProperties": false,
            },
        }),
    ]
}

// ---------------------------------------------------------------- search

/// Lines of text files under `start` (a folder or one file inside `root`)
/// containing every word of `query` (case-insensitive unless `exact`), as
/// `(path:line, text)` with the path relative to `root`. Hidden entries,
/// `target`, `node_modules`, the output folder and binary / large files are
/// skipped. Returns the hits and whether `limit` cut the scan short.
pub fn search_files(
    root: &Path,
    start: &Path,
    skip: &Path,
    query: &str,
    limit: usize,
    exact: bool,
) -> Res<(Vec<(String, String)>, bool)> {
    let fold = |s: &str| if exact { s.to_string() } else { s.to_lowercase() };
    let words: Vec<String> = query.split_whitespace().map(fold).collect();
    let mut hits = Vec::new();
    let mut stack = vec![start.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries: Vec<PathBuf> = if dir.is_file() {
            vec![dir.clone()]
        } else {
            match std::fs::read_dir(&dir) {
                Ok(rd) => rd.flatten().map(|e| e.path()).collect(),
                Err(_) => continue,
            }
        };
        entries.sort();
        let mut subdirs = Vec::new();
        for path in entries {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') || name == "target" || name == "node_modules" || path == skip {
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
            if meta.is_dir() {
                subdirs.push(path);
                continue;
            }
            if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            if text.contains('\0') {
                continue;
            }
            let rel = path.strip_prefix(root).unwrap_or(&path).display().to_string();
            for (n, line) in text.lines().enumerate() {
                let folded = fold(line);
                if words.iter().all(|w| folded.contains(w.as_str())) {
                    if hits.len() == limit {
                        return Ok((hits, true));
                    }
                    hits.push((format!("{rel}:{}", n + 1), clip(line.trim(), MAX_HIT_CHARS)));
                }
            }
        }
        // Depth-first in name order: push reversed so the first name pops first.
        stack.extend(subdirs.into_iter().rev());
    }
    Ok((hits, false))
}

/// Wikipedia `list=search` for titles, then `prop=extracts` for the plain
/// text intros, in search order. `(title, intro)`.
fn search_wikipedia(query: &str, lang: &str, limit: usize) -> Res<Vec<(String, String)>> {
    let api = format!("https://{lang}.wikipedia.org/w/api.php");
    let http = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("ask/", env!("CARGO_PKG_VERSION"), " (MCP pipeline demo)"))
        .build();
    let get = |params: &[(&str, &str)]| -> Res<Value> {
        let mut req = http.get(&api);
        for (k, v) in params {
            req = req.query(k, v);
        }
        req.call()
            .map_err(|e| format!("wikipedia: {e}"))?
            .into_json::<Value>()
            .map_err(|e| format!("wikipedia: {e}"))
    };
    let limit_s = limit.to_string();
    let found = get(&[
        ("action", "query"),
        ("list", "search"),
        ("srsearch", query),
        ("srlimit", &limit_s),
        ("format", "json"),
    ])?;
    let titles: Vec<String> = found["query"]["search"]
        .as_array()
        .map(|a| a.iter().filter_map(|h| h["title"].as_str().map(String::from)).collect())
        .unwrap_or_default();
    if titles.is_empty() {
        return Ok(Vec::new());
    }
    let joined = titles.join("|");
    let pages = get(&[
        ("action", "query"),
        ("prop", "extracts"),
        ("exintro", "1"),
        ("explaintext", "1"),
        ("exlimit", "max"),
        ("titles", &joined),
        ("format", "json"),
    ])?;
    let mut intros: BTreeMap<String, String> = BTreeMap::new();
    if let Some(obj) = pages["query"]["pages"].as_object() {
        for p in obj.values() {
            if let (Some(t), Some(x)) = (p["title"].as_str(), p["extract"].as_str()) {
                let x = x.split_whitespace().collect::<Vec<_>>().join(" ");
                intros.insert(t.to_string(), clip(&x, MAX_WIKI_CHARS));
            }
        }
    }
    Ok(titles
        .into_iter()
        .filter_map(|t| intros.get(&t).filter(|x| !x.is_empty()).map(|x| (t.clone(), x.clone())))
        .collect())
}

fn plural(n: usize, one: &'static str, few: &'static str, many: &'static str) -> &'static str {
    match (n % 10, n % 100) {
        (1, r) if r != 11 => one,
        (2..=4, r) if !(12..=14).contains(&r) => few,
        _ => many,
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------- summarize

pub struct Summary {
    pub text: String,
    pub total: usize,
    pub picked: usize,
    pub keywords: Vec<String>,
}

const STOP: &[&str] = &[
    "this", "that", "with", "from", "have", "were", "which", "their", "there", "about", "into", "than",
    "then", "also", "been", "will", "would", "when", "what", "your", "they", "them", "only", "other",
    "это", "этот", "эта", "эти", "того", "чтобы", "который", "которая", "которые", "также", "если",
    "когда", "было", "были", "была", "будет", "может", "только", "после", "более", "очень", "всех",
    "всего", "есть", "него", "нему", "него", "свой", "своей", "между", "через", "здесь", "каждый",
];

fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|w| w.chars().count() >= 4 && !w.chars().all(|c| c.is_ascii_digit()) && !STOP.contains(&w.as_str()))
        .collect()
}

/// Sentences of the input: one per line for `path:line: text` hits, split
/// on `. ! ?` inside prose; `## Title` lines are headings, not sentences.
fn sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with("## ")) {
        let mut cur = String::new();
        let chars: Vec<char> = line.chars().collect();
        for (i, &c) in chars.iter().enumerate() {
            cur.push(c);
            let end = matches!(c, '.' | '!' | '?') && chars.get(i + 1).is_some_and(|n| n.is_whitespace());
            if end && cur.trim().chars().count() >= 20 {
                out.push(cur.trim().to_string());
                cur.clear();
            }
        }
        if !cur.trim().is_empty() {
            out.push(cur.trim().to_string());
        }
    }
    out
}

/// Deterministic extractive summary: score each sentence by the summed
/// frequency of its distinct content words (normalised by length), keep the
/// `max` best in their original order, head them with the top keywords.
pub fn summarize_text(text: &str, max: usize) -> Summary {
    let sents = sentences(text);
    let mut freq: BTreeMap<String, usize> = BTreeMap::new();
    for s in &sents {
        for w in words(s) {
            *freq.entry(w).or_default() += 1;
        }
    }
    let mut scored: Vec<(usize, f64)> = sents
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let ws: HashSet<String> = words(s).into_iter().collect();
            let sum: usize = ws.iter().map(|w| freq[w]).sum();
            (i, sum as f64 / ((ws.len() + 1) as f64).sqrt())
        })
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
    let mut seen = HashSet::new();
    let mut keep: Vec<usize> = Vec::new();
    for (i, _) in scored {
        if keep.len() == max {
            break;
        }
        if seen.insert(sents[i].clone()) {
            keep.push(i);
        }
    }
    keep.sort_unstable();
    let mut kw: Vec<(&String, &usize)> = freq.iter().collect();
    kw.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    let keywords: Vec<String> = kw.into_iter().take(6).map(|(w, _)| w.clone()).collect();
    if keep.is_empty() {
        return Summary { text: String::new(), total: 0, picked: 0, keywords };
    }
    let mut out = format!("Ключевые слова: {}", keywords.join(", "));
    for i in &keep {
        out.push_str(&format!("\n- {}", clip(&sents[*i], 300)));
    }
    Summary { text: out, total: sents.len(), picked: keep.len(), keywords }
}

// ---------------------------------------------------------------- pipeline

pub struct ChainRequest {
    pub query: String,
    pub source: String,
    pub filename: String,
    pub max_sentences: usize,
}

impl ChainRequest {
    pub fn new(query: &str, source: &str, filename: Option<&str>) -> ChainRequest {
        ChainRequest {
            query: query.trim().to_string(),
            source: source.to_string(),
            filename: filename.map(String::from).unwrap_or_else(|| default_filename(query)),
            max_sentences: 5,
        }
    }
}

/// `pipeline-<slug of the query>.md`.
pub fn default_filename(query: &str) -> String {
    let slug: String = query
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug.split('-').filter(|s| !s.is_empty()).collect::<Vec<_>>().join("-");
    let slug: String = slug.chars().take(40).collect();
    format!("pipeline-{}.md", if slug.is_empty() { "result" } else { &slug })
}

pub struct ChainReport {
    /// Hand-offs checked / hand-offs that matched.
    pub handoffs: usize,
    pub handoffs_ok: usize,
    pub summary: String,
    pub path: String,
    /// The file on disk is byte-identical to what summarize returned.
    pub file_ok: bool,
}

impl ChainReport {
    pub fn ok(&self) -> bool {
        self.handoffs == 2 && self.handoffs_ok == 2 && self.file_ok
    }
}

/// Long data in a shown argument list becomes `<N симв #digest>`.
pub fn shown_args(args: &Value) -> String {
    shown_args_within(args, 60)
}

/// [`shown_args`] with strings up to `max` chars shown verbatim.
pub fn shown_args_within(args: &Value, max: usize) -> String {
    let mut a = args.clone();
    if let Some(obj) = a.as_object_mut() {
        for v in obj.values_mut() {
            if let Some(s) = v.as_str() {
                if s.chars().count() > max {
                    *v = json!(format!("<{} симв #{}>", s.chars().count(), short(&digest(s))));
                }
            }
        }
    }
    a.to_string()
}

fn call_step(
    caller: &mut dyn ToolCaller,
    n: usize,
    tool: &str,
    args: Value,
    emit: &mut dyn FnMut(String),
) -> Res<CallResult> {
    emit(format!("шаг {n}/3 · {tool} {}", shown_args(&args)));
    let r = caller.call_tool(tool, args)?;
    let head = split_header(&r.text).0.unwrap_or(&r.text);
    emit(format!("   ← {}{head}", if r.is_error { "ERROR " } else { "" }));
    if r.is_error {
        return Err(format!("{tool}: {}", r.text));
    }
    Ok(r)
}

/// The automatic pipeline: search → summarize → saveToFile. Every step is
/// a `tools/call`; the client carries the data from one tool's output to
/// the next tool's input *by value* and checks each hand-off: the digest of
/// what it sent must equal the producer's output digest and the digest the
/// receiver says it got. At the end the file is read back from disk.
/// `emit` gets every line as it happens.
pub fn run_chain(caller: &mut dyn ToolCaller, req: &ChainRequest, emit: &mut dyn FnMut(String)) -> Res<ChainReport> {
    let say = emit;
    say(format!(
        "пайплайн search → summarize → saveToFile · запрос «{}» · источник {} · файл {}",
        req.query, req.source, req.filename
    ));

    let found = call_step(
        caller,
        1,
        "search",
        json!({"query": req.query, "source": req.source}),
        say,
    )?;
    if found.structured["count"].as_u64().unwrap_or(0) == 0 {
        say("итог: search ничего не нашёл — цепочка остановлена на шаге 1, дальше передавать нечего".into());
        return Err(format!("search found nothing for «{}»", req.query));
    }
    let data1 = split_header(&found.text).1.to_string();
    let out1 = found.structured["digest"].as_str().unwrap_or("").to_string();
    let sent1 = digest(&data1);

    let mut handoffs_ok = 0;
    let handoff = |say: &mut dyn FnMut(String), from: &str, to: &str, sent: &str, out: &str, got: &Value| {
        let got = got["digest"].as_str().unwrap_or("");
        let ok = sent == out && got == out;
        say(format!(
            "   передача {from} → {to}: отправлено #{}, выход {from} #{}, {to} получил #{} {}",
            short(sent),
            short(out),
            short(got),
            if ok { "✓" } else { "✗ ДАННЫЕ ИСКАЖЕНЫ" }
        ));
        ok
    };

    say(format!(
        "   данные шага 1 → summarize.text ({} симв, #{})",
        data1.chars().count(),
        short(&sent1)
    ));
    let summed = call_step(
        caller,
        2,
        "summarize",
        json!({"text": data1, "max_sentences": req.max_sentences}),
        say,
    )?;
    if handoff(say, "search", "summarize", &sent1, &out1, &summed.structured["input"]) {
        handoffs_ok += 1;
    }
    let data2 = split_header(&summed.text).1.to_string();
    let out2 = summed.structured["digest"].as_str().unwrap_or("").to_string();
    let sent2 = digest(&data2);

    say(format!(
        "   данные шага 2 → saveToFile.content ({} симв, #{})",
        data2.chars().count(),
        short(&sent2)
    ));
    let saved = call_step(
        caller,
        3,
        "saveToFile",
        json!({"filename": req.filename, "content": data2}),
        say,
    )?;
    if handoff(say, "summarize", "saveToFile", &sent2, &out2, &saved.structured["input"]) {
        handoffs_ok += 1;
    }
    let path = saved.structured["path"].as_str().unwrap_or("").to_string();
    let on_disk = std::fs::read_to_string(&path).map_err(|e| format!("read back {path}: {e}"))?;
    let file_ok = on_disk == data2;
    say(format!(
        "   файл {path}: {} байт, #{} — {}",
        on_disk.len(),
        short(&digest(&on_disk)),
        if file_ok { "побайтно равен выходу summarize ✓" } else { "НЕ равен выходу summarize ✗" }
    ));
    let report = ChainReport {
        handoffs: 2,
        handoffs_ok,
        summary: data2,
        path,
        file_ok,
    };
    say(if report.ok() {
        "итог: 3 инструмента отработали автоматически, обе передачи сверены по дайджестам, файл на диске совпал".into()
    } else {
        "итог: цепочка прошла, но передача данных НЕ сошлась — см. ✗ выше".into()
    });
    Ok(report)
}

/// Hand-off audit of a chain someone else assembled (the model in the
/// chat): every `summarize` / `saveToFile` step must have received exactly
/// the output of an earlier step — a pipeline tool or any other MCP tool
/// such as `git_log` (digest match). Empty when the turn didn't touch the
/// pipeline tools.
pub fn audit(steps: &[ToolStep]) -> Vec<String> {
    if !steps.iter().any(|s| TOOLS.contains(&s.name.as_str())) {
        return Vec::new();
    }
    let mut outputs: Vec<(String, String)> = Vec::new(); // (digest, "search r1")
    let mut lines = vec![format!(
        "цепочка MCP: {}",
        steps.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(" → ")
    )];
    for s in steps {
        let st = &s.structured;
        if s.is_error {
            lines.push(format!("✗ {}: ошибка инструмента — {}", s.name, mcp_agent::preview(&s.result)));
            continue;
        }
        if !TOOLS.contains(&s.name.as_str()) {
            outputs.push((digest(split_header(&s.result).1), s.name.clone()));
            continue;
        }
        let id = st["id"].as_str().unwrap_or("?");
        if s.name != "search" {
            let got = st["input"]["digest"].as_str().unwrap_or("");
            let via = st["input"]["via"].as_str().unwrap_or("?");
            match outputs.iter().rev().find(|(d, _)| d == got) {
                Some((_, from)) => lines.push(format!(
                    "✓ {} {id} получил выход {from} без искажений (#{}, передано через {via})",
                    s.name,
                    short(got)
                )),
                None => lines.push(format!(
                    "✗ {} {id}: вход #{} не совпадает побайтно ни с одним выходом предыдущих шагов (передано через {via}) — данные изменены при передаче",
                    s.name,
                    short(got)
                )),
            }
        }
        if let Some(d) = st["digest"].as_str() {
            outputs.push((d.to_string(), format!("{} {id}", s.name)));
        }
    }
    lines
}

// ---------------------------------------------------------------- verify

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Corpus for tests and the proof: three lines about quasars — one of them
/// with `codename` — among unrelated files.
pub fn fixture_corpus(dir: &Path, codename: &str) -> Res<()> {
    let w = |p: &str, t: &str| -> Res<()> {
        let path = dir.join(p);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, t).map_err(|e| e.to_string())
    };
    w(
        "notes/astro.md",
        "# Заметки\nКвазар — активное ядро далёкой галактики, одно из самых ярких тел во Вселенной.\n\
         Излучение квазара питает аккреция вещества на сверхмассивную чёрную дыру.\n\
         Разное: купить хлеб.\n",
    )?;
    w("notes/obs.txt", &format!(
        "Наблюдение 12: квазар получил кодовое имя {codename} в журнале обсерватории.\n\
         Погода была ясной.\n"
    ))?;
    w("src/main.rs", "fn main() { println!(\"no stars here\"); }\n")?;
    w("README.md", "Корпус для проверки пайплайна MCP.\n")
}

fn stamp() -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:08x}", (n as u32) ^ std::process::id().rotate_left(16))
}

fn mark(ok: bool) -> &'static str {
    if ok {
        "ok"
    } else {
        "FAIL"
    }
}

/// `ask --verify-pipeline offline|live|all`.
///
/// * offline — the automatic chain over HTTP on a fixture corpus with a
///   random codename: the codename must travel search → summarize → file,
///   both hand-offs must match by digest, the file must equal an
///   independent local `summarize_text(search output)`; and two controls
///   must fail loudly (a dangling `input_id`, a tampered hand-off caught by
///   the audit) — otherwise the check would be vacuous.
/// * live — the *model* gets one sentence ("найди, сверни, сохрани") and
///   the pipeline tools: Confirmed only if the server logged search →
///   summarize → saveToFile in that order, every hand-off matched, and the
///   file on disk holds the codename and equals what summarize returned.
pub fn verify(which: &str, settings: &Settings) -> Res<bool> {
    let (offline, live) = match which {
        "offline" => (true, false),
        "live" => (false, true),
        "all" | "" => (true, true),
        other => return Err(format!("--verify-pipeline: unknown {other:?} (offline|live|all)")),
    };
    let code = stamp();
    let codename = format!("KV-{code}");
    let base = TempDir(std::env::temp_dir().join(format!("ask-pipeline-proof-{code}")));
    let corpus = base.0.join("corpus");
    let out = base.0.join("out");
    fixture_corpus(&corpus, &codename)?;
    let server = Server::new(&corpus, &out, false)?;
    let url = server.spawn(0)?;
    let mut conn = Connection::connect(&url)?;
    let tools = conn.list_tools()?;
    println!(
        "сервер: {} {} на {url}, инструменты: {}",
        conn.server_name,
        conn.server_version,
        tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", ")
    );
    println!("корпус: {} (кодовое имя {codename} только в notes/obs.txt)", corpus.display());
    let mut all_ok = true;

    if offline {
        println!("\n== offline: автоматическая цепочка ==");
        let req = ChainRequest::new("квазар", "files", Some(&format!("proof-{code}.md")));
        let rep = run_chain(&mut conn, &req, &mut |l| println!("{l}"))?;
        let found = conn.call_tool("search", json!({"query": "квазар"}))?;
        let expected = summarize_text(split_header(&found.text).1, req.max_sentences).text;
        let on_disk = std::fs::read_to_string(&rep.path).unwrap_or_default();
        let independent = on_disk == expected;
        let carried = on_disk.contains(&codename);
        println!("[{}] передачи сверены: {}/{}", mark(rep.handoffs_ok == 2), rep.handoffs_ok, rep.handoffs);
        println!("[{}] файл = локальный summarize_text(выход search), посчитанный отдельно", mark(independent));
        println!("[{}] кодовое имя дошло от search до файла на диске", mark(carried));

        let dangling = conn.call_tool("summarize", json!({"input_id": "r999"}))?;
        println!(
            "[{}] контроль 1: summarize{{input_id:r999}} → isError ({})",
            mark(dangling.is_error),
            dangling.text
        );
        // Control 2: a hand-off that changed on the way. The audit must say ✗.
        let s1 = conn.call_tool("search", json!({"query": "квазар"}))?;
        let tampered = split_header(&s1.text).1.replace(&codename, "KV-00000000");
        let s2 = conn.call_tool("summarize", json!({"text": tampered}))?;
        let step = |name: &str, r: CallResult| ToolStep {
            name: name.into(),
            server: SERVER_NAME.into(),
            args: Value::Null,
            result: r.text,
            is_error: r.is_error,
            structured: r.structured,
        };
        let lines = audit(&[step("search", s1), step("summarize", s2)]);
        let caught = lines.iter().any(|l| l.starts_with('✗'));
        println!("[{}] контроль 2: подменённая передача search → summarize поймана аудитом", mark(caught));
        for l in &lines {
            println!("    {l}");
        }
        let ok = rep.ok() && independent && carried && dangling.is_error && caught;
        println!("offline: {}", if ok { "Confirmed" } else { "Flat" });
        all_ok &= ok;
    }

    if live {
        println!("\n== live: цепочку собирает модель {} ==", settings.model);
        server.calls.lock().map_err(|e| e.to_string())?.clear();
        let file = format!("live-{code}.md");
        let question = format!(
            "Найди в файлах всё про квазар, сделай краткую сводку и сохрани её в файл {file}. \
             Потом коротко скажи, что сохранил и куда."
        );
        println!("вопрос: {question}");
        let ep = Endpoint::for_model(&settings.model)?;
        let mut tb = Toolbox::connect(&url, format!("pipeline ({url})"))?;
        let system = format!(
            "You are an agent with MCP tools. Answer in the language of the question.\n\n{}",
            mcp_agent::chat_note(&tb)
        );
        let functions = tb.functions.clone();
        let (outcome, steps, rounds) = mcp_agent::tool_loop(
            &ep,
            settings,
            &system,
            vec![json!({"role": "user", "content": question})],
            &mut tb,
            &functions,
            &mut |s| {
                println!(
                    "    MCP → {} {}  ← {}{}",
                    s.name,
                    shown_args(&s.args),
                    if s.is_error { "ERROR " } else { "" },
                    mcp_agent::preview(&s.result)
                )
            },
        )?;
        let logged: Vec<String> = server
            .calls
            .lock()
            .map_err(|e| e.to_string())?
            .iter()
            .map(|c| c.split_whitespace().next().unwrap_or("").to_string())
            .collect();
        let pos = |t: &str| logged.iter().position(|c| c == t);
        let ordered = matches!((pos("search"), pos("summarize"), pos("saveToFile")), (Some(a), Some(b), Some(c)) if a < b && b < c);
        println!("[{}] сервер записал вызовы: {}", mark(ordered), logged.join(" → "));
        let lines = audit(&steps);
        let handoffs_ok = !lines.is_empty() && lines.iter().skip(1).all(|l| l.starts_with('✓'));
        println!("[{}] аудит передач:", mark(handoffs_ok));
        for l in &lines {
            println!("    {l}");
        }
        let path = out.join(&file);
        let on_disk = std::fs::read_to_string(&path).ok();
        let last_summary = steps
            .iter()
            .rev()
            .find(|s| s.name == "summarize" && !s.is_error)
            .map(|s| split_header(&s.result).1.to_string());
        let file_ok = on_disk.is_some() && on_disk == last_summary;
        let carried = on_disk.as_deref().is_some_and(|t| t.contains(&codename));
        println!(
            "[{}] файл {} существует и равен выходу summarize",
            mark(file_ok),
            path.display()
        );
        println!("[{}] кодовое имя {codename} в файле", mark(carried));
        println!(
            "    ответ модели: {}",
            outcome.text().split_whitespace().collect::<Vec<_>>().join(" ")
        );
        println!(
            "    раундов {} [{}]",
            rounds.len(),
            rounds
                .iter()
                .map(|r| format!("{} ({}→{})", r.finish_reason, r.prompt_tokens, r.completion_tokens))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let ok = ordered && handoffs_ok && file_ok && carried;
        println!("live: {}", if ok { "Confirmed" } else { "Flat" });
        all_ok &= ok;
    }
    Ok(all_ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(tag: &str) -> (TempDir, Server) {
        let base = TempDir(std::env::temp_dir().join(format!("ask-toolchain-{tag}-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&base.0);
        fixture_corpus(&base.0.join("corpus"), "KV-TEST").unwrap();
        let s = Server::new(&base.0.join("corpus"), &base.0.join("out"), false).unwrap();
        (base, s)
    }

    fn call(s: &Server, name: &str, args: Value) -> Value {
        s.handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                         "params": {"name": name, "arguments": args}}))
            .unwrap()["result"]
            .clone()
    }

    #[test]
    fn three_tools_with_schemas() {
        let (_d, s) = fixture("list");
        let r = s.handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})).unwrap();
        let names: Vec<&str> = r["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, TOOLS);
    }

    #[test]
    fn search_finds_lines_with_all_words() {
        let (_d, s) = fixture("search");
        let r = call(&s, "search", json!({"query": "квазар"}));
        assert_eq!(r["isError"], false);
        assert_eq!(r["structuredContent"]["count"], 3);
        let text = r["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("[r1] search «квазар»"), "{text}");
        assert!(text.contains("notes/obs.txt:1: Наблюдение 12") && text.contains("KV-TEST"), "{text}");
        let r = call(&s, "search", json!({"query": "квазар имя"}));
        assert_eq!(r["structuredContent"]["count"], 1);
        let r = call(&s, "search", json!({"query": "квазар", "limit": 1}));
        assert_eq!(r["structuredContent"]["truncated"], true);
        let r = call(&s, "search", json!({"query": "x", "source": "ftp"}));
        assert_eq!(r["isError"], true);
        // `path` narrows to a folder or one file; hits stay relative to the root.
        let r = call(&s, "search", json!({"query": "квазар", "path": "notes"}));
        let hits = r["structuredContent"]["hits"].as_array().unwrap().clone();
        assert!(!hits.is_empty() && hits.iter().all(|h| h["where"].as_str().unwrap().starts_with("notes/")), "{hits:?}");
        let r = call(&s, "search", json!({"query": "квазар", "path": "./notes/obs.txt"}));
        assert_eq!(r["structuredContent"]["hits"][0]["where"], "notes/obs.txt:1");
        assert_eq!(call(&s, "search", json!({"query": "x", "path": "../"}))["isError"], true);
        assert_eq!(call(&s, "search", json!({"query": "x", "path": "nope"}))["isError"], true);
    }

    #[test]
    fn handoff_by_id_and_by_value_carry_the_same_digest() {
        let (_d, s) = fixture("handoff");
        let found = call(&s, "search", json!({"query": "квазар"}));
        let out1 = found["structuredContent"]["digest"].clone();
        let by_id = call(&s, "summarize", json!({"input_id": "r1"}));
        assert_eq!(by_id["structuredContent"]["input"]["digest"], out1);
        assert_eq!(by_id["structuredContent"]["input"]["via"], "input_id");
        // Header copied along with the text is stripped, data is the same.
        let by_value = call(&s, "summarize", json!({"text": found["content"][0]["text"]}));
        assert_eq!(by_value["structuredContent"]["input"]["digest"], out1);
        assert_eq!(by_id["structuredContent"]["digest"], by_value["structuredContent"]["digest"]);
        let saved = call(&s, "saveToFile", json!({"input_id": "r2", "filename": "a.md"}));
        assert_eq!(saved["structuredContent"]["digest"], by_id["structuredContent"]["digest"]);
        let path = saved["structuredContent"]["path"].as_str().unwrap();
        assert!(std::fs::read_to_string(path).unwrap().contains("KV-TEST"));
        for bad in ["../x.md", "a/b.md", ".hidden", ""] {
            let r = call(&s, "saveToFile", json!({"input_id": "r2", "filename": bad}));
            assert_eq!(r["isError"], true, "{bad}");
        }
        let r = call(&s, "summarize", json!({"input_id": "r1", "text": "x"}));
        assert_eq!(r["isError"], true);
    }

    #[test]
    fn summary_is_deterministic_and_ordered() {
        let text = "Кошки любят рыбу и молоко. Собаки охраняют дом. Кошки спят весь день на солнце. \
                    Рыба бывает морской. Кошки ловят мышей в доме.";
        let a = summarize_text(text, 2);
        assert_eq!(a.text, summarize_text(text, 2).text);
        assert_eq!(a.total, 5);
        assert_eq!(a.picked, 2);
        assert_eq!(a.keywords[0], "кошки");
        let body: Vec<&str> = a.text.lines().skip(1).collect();
        assert_eq!(body.len(), 2);
        assert!(body.iter().all(|l| l.contains("Кошки")), "{body:?}");
        assert!(summarize_text("", 3).text.is_empty());
    }

    #[test]
    fn chain_over_http_and_audit() {
        let (_d, s) = fixture("chain");
        let url = s.spawn(0).unwrap();
        let mut conn = Connection::connect(&url).unwrap();
        assert_eq!(conn.server_name, SERVER_NAME);
        let req = ChainRequest::new("квазар", "files", Some("chain.md"));
        let mut seen = Vec::new();
        let rep = run_chain(&mut conn, &req, &mut |l| seen.push(l)).unwrap();
        assert!(rep.ok(), "{seen:#?}");
        assert!(seen.iter().any(|l| l.starts_with("шаг 3/3 · saveToFile")));
        assert!(rep.summary.contains("KV-TEST"));
        assert_eq!(s.calls.lock().unwrap().len(), 3);

        let none = run_chain(&mut conn, &ChainRequest::new("пульсар", "files", None), &mut |_| {});
        assert!(none.is_err());

        // audit: honest chain ✓, retyped-and-changed hand-off ✗
        let step = |name: &str, r: CallResult| ToolStep {
            name: name.into(),
            server: SERVER_NAME.into(),
            args: Value::Null,
            result: r.text,
            is_error: r.is_error,
            structured: r.structured,
        };
        let a = conn.call_tool("search", json!({"query": "квазар"})).unwrap();
        let id = a.structured["id"].as_str().unwrap().to_string();
        let b = conn.call_tool("summarize", json!({"input_id": id})).unwrap();
        let c = conn.call_tool("summarize", json!({"text": "Квазар — это что-то другое совсем."})).unwrap();
        let lines = audit(&[step("search", a), step("summarize", b), step("summarize", c)]);
        assert_eq!(lines[0], "цепочка MCP: search → summarize → summarize");
        assert!(lines[1].starts_with("✓ summarize"), "{lines:?}");
        assert!(lines[2].starts_with("✗ summarize"), "{lines:?}");
        assert!(audit(&[]).is_empty());

        // A non-pipeline tool (git_log) feeding summarize by value counts as a producer.
        let git = ToolStep {
            name: "git_log".into(),
            server: "ask-git-mcp".into(),
            args: Value::Null,
            result: "abc1234 2026-09-27 Carol: fix: the answer is 42".into(),
            is_error: false,
            structured: Value::Null,
        };
        let d = conn.call_tool("summarize", json!({"text": git.result})).unwrap();
        let lines = audit(&[git, step("summarize", d)]);
        assert!(lines[1].starts_with("✓ summarize") && lines[1].contains("git_log"), "{lines:?}");
    }

    #[test]
    fn default_filename_is_a_valid_name() {
        let f = default_filename("Что такое MCP?");
        assert_eq!(f, "pipeline-что-такое-mcp.md");
        assert!(check_filename(&f).is_ok());
        assert_eq!(default_filename("!!!"), "pipeline-result.md");
        assert_eq!(plural(1, "a", "b", "c"), "a");
        assert_eq!(plural(3, "a", "b", "c"), "b");
        assert_eq!(plural(12, "a", "b", "c"), "c");
        assert_eq!(plural(21, "a", "b", "c"), "a");
    }
}
