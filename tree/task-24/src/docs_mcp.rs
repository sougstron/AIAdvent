//! Task 22: `ask-docs-mcp` — the documents of a folder read *raw*, without
//! the index. The same corpus RAG retrieves from (`docs/*.pdf|md|txt`),
//! opened through the same loader (`rag::load_file`: pdftotext, pages,
//! sections), so "the model read it itself" and "RAG handed it the chunks"
//! are compared on identical text.
//!
//! * `docs_list` — files with pages, size, title and section names;
//! * `docs_read` — the text of a page range (PDF) or of the whole file,
//!   capped by `max_chars`, with page markers and a hint where to continue;
//! * `docs_search` — lines that contain every word of a query, with pages.
//!
//! Only plain file names from the listing are accepted: no paths, nothing
//! outside the folder.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::config::Res;
use crate::mcp_server::{self, CallLog, ServerInfo};
use crate::rag::{self, Doc};
use crate::toolchain::digest;

pub const SERVER_NAME: &str = "ask-docs-mcp";
pub const TOOLS: [&str; 3] = ["docs_list", "docs_read", "docs_search"];
/// The chat client hands the model at most 12 000 characters of a tool
/// result (`mcp_agent::MAX_TOOL_CHARS`); a page is cut below that, so the
/// header and the "continue from" hint always reach the model.
const READ_DEFAULT: usize = 10_000;
const READ_MAX: usize = 11_000;
const SEARCH_DEFAULT: usize = 20;

#[derive(Clone)]
pub struct Server {
    dir: PathBuf,
    cache: Arc<Mutex<HashMap<String, Arc<Doc>>>>,
    pub calls: CallLog,
    verbose: bool,
}

impl Server {
    pub fn new(dir: &Path, verbose: bool) -> Res<Server> {
        let dir = dir.canonicalize().map_err(|e| format!("docs dir {}: {e}", dir.display()))?;
        if !dir.is_dir() {
            return Err(format!("{} is not a folder", dir.display()));
        }
        Ok(Server { dir, cache: Arc::default(), calls: Arc::default(), verbose })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn serve(&self, listener: TcpListener) {
        for stream in listener.incoming().flatten() {
            let r = mcp_server::handle_connection(stream, "mcp-docs", self.verbose, &|m| self.handle(m));
            if let (Err(e), true) = (r, self.verbose) {
                eprintln!("[mcp-docs] connection error: {e}");
            }
        }
    }

    pub fn spawn(&self, port: u16) -> Res<String> {
        let me = self.clone();
        crate::orchestra::spawn_on(port, move |l| me.serve(l))
    }

    pub fn handle(&self, msg: &Value) -> Option<Value> {
        let info = ServerInfo {
            name: SERVER_NAME,
            instructions: format!(
                "Raw documents of the folder {} (PDF, Markdown, text). To answer a question about them: \
                 `docs_list` to see files, pages and section names, then `docs_read` the pages that hold \
                 the answer (or `docs_search` for a term first). Cite the file and pages you used.",
                self.dir.display()
            ),
        };
        mcp_server::dispatch(msg, &info, &tool_specs(), &self.calls, &|name, args| self.call(name, args))
    }

    fn call(&self, name: &str, args: &Value) -> Res<(String, Value)> {
        match name {
            "docs_list" => self.list(),
            "docs_read" => self.read(args),
            "docs_search" => self.search(args),
            _ => Err(format!("unknown tool: {name}")),
        }
    }

    fn files(&self) -> Res<Vec<String>> {
        let mut names: Vec<String> = std::fs::read_dir(&self.dir)
            .map_err(|e| format!("{}: {e}", self.dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
                p.is_file() && matches!(ext.as_str(), "pdf" | "md" | "txt")
            })
            .filter_map(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
            .collect();
        names.sort();
        Ok(names)
    }

    fn doc(&self, file: &str) -> Res<Arc<Doc>> {
        let files = self.files()?;
        if !files.iter().any(|f| f == file) {
            return Err(format!("no document {file:?} here; files: {}", files.join(", ")));
        }
        let mut cache = self.cache.lock().map_err(|e| e.to_string())?;
        if let Some(d) = cache.get(file) {
            return Ok(d.clone());
        }
        let d = Arc::new(rag::load_file(&self.dir.join(file))?);
        cache.insert(file.to_string(), d.clone());
        Ok(d)
    }

    fn list(&self) -> Res<(String, Value)> {
        let mut lines = Vec::new();
        let mut items = Vec::new();
        for f in self.files()? {
            let d = self.doc(&f)?;
            let sections: Vec<String> = d
                .sections
                .iter()
                .map(|s| format!("{} (стр. {})", s.path, page_of(&d, s.start)))
                .collect();
            lines.push(format!(
                "{f} — «{}», {} стр., {} симв.\n  разделы: {}",
                d.title,
                d.pages.len(),
                d.chars.len(),
                sections.join("; ")
            ));
            items.push(json!({"file": f, "title": d.title, "pages": d.pages.len(),
                              "chars": d.chars.len(), "sections": sections}));
        }
        if lines.is_empty() {
            lines.push("(документов нет)".into());
        }
        Ok((lines.join("\n"), json!({"files": items})))
    }

    fn read(&self, args: &Value) -> Res<(String, Value)> {
        let file = req_str(args, "file")?;
        let d = self.doc(file)?;
        let n = d.pages.len();
        let page = |key: &str, default: usize| -> Res<usize> {
            match &args[key] {
                Value::Null => Ok(default),
                v => v
                    .as_u64()
                    .map(|p| p as usize)
                    .filter(|p| (1..=n).contains(p))
                    .ok_or_else(|| format!("`{key}` must be a page number from 1 to {n}")),
            }
        };
        let from = page("from_page", 1)?;
        let to = page("to_page", n)?;
        if to < from {
            return Err(format!("`to_page` {to} is before `from_page` {from}"));
        }
        let max = match &args["max_chars"] {
            Value::Null => READ_DEFAULT,
            v => v
                .as_u64()
                .map(|m| m as usize)
                .filter(|m| (500..=READ_MAX).contains(m))
                .ok_or_else(|| format!("`max_chars` must be from 500 to {READ_MAX}"))?,
        };
        let mut out = String::new();
        let mut last = from;
        let mut truncated = false;
        for p in from..=to {
            let (a, b) = page_span(&d, p);
            let body: String = d.chars[a..b].iter().collect();
            let piece = if n > 1 { format!("--- стр. {p} ---\n{}\n", body.trim_end()) } else { body };
            if !out.is_empty() && out.chars().count() + piece.chars().count() > max {
                truncated = true;
                break;
            }
            out += &piece;
            last = p;
            if out.chars().count() > max {
                out = out.chars().take(max).collect();
                truncated = true;
                break;
            }
        }
        let (a, _) = page_span(&d, from);
        let (_, b) = page_span(&d, last);
        let sections: Vec<String> = d
            .sections
            .iter()
            .filter(|s| s.start < b && a < s.end)
            .map(|s| s.path.clone())
            .collect();
        let more = if truncated && last < n {
            format!("\n[обрезано по max_chars={max}: прочитаны стр. {from}–{last} из {n}; дальше — from_page={}]", last + 1)
        } else if truncated {
            format!("\n[обрезано по max_chars={max}]")
        } else {
            String::new()
        };
        let header = format!("{file}: стр. {from}–{last} из {n}, {} симв.\n", out.chars().count());
        let d_out = digest(&out);
        Ok((
            format!("{header}{out}{more}"),
            json!({"file": file, "from_page": from, "to_page": last, "pages": n,
                   "chars": out.chars().count(), "truncated": truncated,
                   "sections": sections, "digest": d_out}),
        ))
    }

    fn search(&self, args: &Value) -> Res<(String, Value)> {
        let query = req_str(args, "query")?;
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        let limit = match &args["limit"] {
            Value::Null => SEARCH_DEFAULT,
            v => v.as_u64().filter(|n| (1..=100).contains(n)).ok_or("`limit` must be from 1 to 100")? as usize,
        };
        let files = match args["file"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
            Some(f) => vec![f.to_string()],
            None => self.files()?,
        };
        let mut hits = Vec::new();
        let mut total = 0;
        for f in &files {
            let d = self.doc(f)?;
            let mut off = 0;
            let text: String = d.chars.iter().collect();
            for line in text.split_inclusive('\n') {
                let low = line.to_lowercase();
                if words.iter().all(|w| low.contains(w.as_str())) {
                    total += 1;
                    if hits.len() < limit {
                        hits.push(json!({"file": f, "page": page_of(&d, off), "line": line.trim()}));
                    }
                }
                off += line.chars().count();
            }
        }
        let text = if hits.is_empty() {
            format!("«{query}»: ничего не найдено")
        } else {
            let mut lines: Vec<String> = hits
                .iter()
                .map(|h| {
                    format!("{} стр.{}: {}", h["file"].as_str().unwrap_or(""), h["page"], h["line"].as_str().unwrap_or(""))
                })
                .collect();
            if total > hits.len() {
                lines.push(format!("… ещё {} строк (limit={limit})", total - hits.len()));
            }
            lines.join("\n")
        };
        Ok((text, json!({"query": query, "total": total, "hits": hits})))
    }
}

fn req_str<'a>(args: &'a Value, key: &str) -> Res<&'a str> {
    args[key]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("`{key}` (string) is required"))
}

fn page_of(d: &Doc, off: usize) -> usize {
    d.pages.iter().rposition(|&p| p <= off).map(|i| i + 1).unwrap_or(1)
}

/// Char span `[start, end)` of page `p` (1-based).
fn page_span(d: &Doc, p: usize) -> (usize, usize) {
    let a = d.pages.get(p - 1).copied().unwrap_or(0);
    let b = d.pages.get(p).copied().unwrap_or(d.chars.len());
    (a, b)
}

pub fn tool_specs() -> Vec<Value> {
    vec![
        json!({
            "name": "docs_list",
            "description": "List the documents of the folder: file name, title, pages, size and section names with their pages.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
        }),
        json!({
            "name": "docs_read",
            "description": "Read the raw text of a document, a page range at a time (PDF pages; .md/.txt are one page). Long ranges are cut at max_chars with a hint where to continue.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {"type": "string", "description": "File name from docs_list, e.g. paper.pdf."},
                    "from_page": {"type": "integer", "minimum": 1, "description": "First page, default 1."},
                    "to_page": {"type": "integer", "minimum": 1, "description": "Last page, default the last one."},
                    "max_chars": {"type": "integer", "minimum": 500, "maximum": READ_MAX, "description": "Cap on returned text, default 10000."},
                },
                "required": ["file"],
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "docs_search",
            "description": "Find lines that contain every word of the query (case-insensitive), with file and page.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "file": {"type": "string", "description": "Only this file (default: all)."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 100, "description": "Default 20."},
                },
                "required": ["query"],
                "additionalProperties": false,
            },
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (PathBuf, Server) {
        let dir = std::env::temp_dir().join(format!("ask-docs-mcp-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.md"), "# Guide\nintro\n## Setup\nInstall the KV-77 widget first.\n").unwrap();
        std::fs::write(dir.join("skip.json"), "{}").unwrap();
        let s = Server::new(&dir, false).unwrap();
        (dir, s)
    }

    #[test]
    fn list_read_search_over_http() {
        let (dir, server) = fixture();
        let url = server.spawn(0).unwrap();
        let mut conn = crate::mcp::Connection::connect(&url).unwrap();
        assert_eq!(conn.server_name, SERVER_NAME);
        let names: Vec<String> = conn.list_tools().unwrap().into_iter().map(|t| t.name).collect();
        assert_eq!(names, TOOLS);
        let list = conn.call_tool("docs_list", json!({})).unwrap();
        assert!(list.text.contains("a.md") && list.text.contains("Guide > Setup") && !list.text.contains("skip.json"));
        let read = conn.call_tool("docs_read", json!({"file": "a.md"})).unwrap();
        assert!(!read.is_error && read.text.contains("KV-77"), "{}", read.text);
        assert_eq!(read.structured["sections"], json!(["Guide", "Guide > Setup"]));
        let found = conn.call_tool("docs_search", json!({"query": "kv-77 widget"})).unwrap();
        assert!(found.text.contains("a.md стр.1: Install the KV-77 widget first."), "{}", found.text);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_paths_and_bad_pages() {
        let (dir, server) = fixture();
        assert!(server.call("docs_read", &json!({"file": "../etc/passwd"})).is_err());
        assert!(server.call("docs_read", &json!({"file": "skip.json"})).is_err());
        assert!(server.call("docs_read", &json!({"file": "a.md", "from_page": 2})).is_err());
        assert!(server.call("docs_read", &json!({"file": "a.md", "max_chars": 10})).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_cuts_by_pages_and_says_where_to_continue() {
        let text = "one\n".repeat(300) + &"two\n".repeat(300) + &"three\n".repeat(300);
        let d = Doc::from_text("x.txt", &text, vec![0, 1200, 2400], false);
        let (dir, server) = fixture();
        server.cache.lock().unwrap().insert("a.md".into(), Arc::new(d));
        let (out, st) = server.read(&json!({"file": "a.md", "max_chars": 1500})).unwrap();
        assert_eq!((st["from_page"].as_u64(), st["to_page"].as_u64()), (Some(1), Some(1)));
        assert!(st["truncated"].as_bool().unwrap());
        assert!(out.contains("from_page=2"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
