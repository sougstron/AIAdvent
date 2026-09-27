//! Task 20: orchestration of several MCP servers in one long flow.
//!
//! Two more own servers join git (task 17) and the pipeline (task 19):
//!
//! * `ask-tracker-mcp` ([`Tracker`]) — an issue tracker in SQLite:
//!   `issue_create`, `issue_list`, `issue_close`;
//! * `ask-notify-mcp` ([`Notify`]) — team notifications into an outbox
//!   file: `notify_send`, `notify_list`.
//!
//! The flow they make together is a TODO triage, every step on the server
//! that owns the tool and every step fed by an earlier one:
//!
//! ```text
//! pipeline.search «TODO:»  →  git.git_log{path} ×files  →  tracker.issue_create ×hits
//!   →  tracker.issue_list  →  pipeline.saveToFile{content = issue_list}  →  notify.notify_send{path}
//! ```
//!
//! [`run_triage`] runs it automatically (`/triage`, `ask --triage`), the
//! model assembles it itself from a plain sentence in the chat, and
//! [`check_flow`] audits either: routing (each call reached the tool's
//! owner), data provenance (the issue's source came from `search`, the
//! assignee from `git_log` of that very file, the saved file is the
//! `issue_list` output byte for byte, the message carries the saved path)
//! and therefore order. [`render`] draws the calls as lanes, one per server.
//! [`verify`] is the causal proof (`ask --verify-orchestra`).

use rusqlite::{params, Connection as Db, OptionalExtension};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::api::Endpoint;
use crate::config::{Res, Settings};
use crate::mcp_agent::{self, ToolCaller, ToolStep, Toolbox};
use crate::mcp_server::{self, CallLog, ServerInfo};
use crate::scheduler::{fmt_time, now};
use crate::toolchain::{self, digest, short, split_header};

pub const GIT: &str = "ask-git-mcp";
pub const TRACKER: &str = "ask-tracker-mcp";
pub const NOTIFY: &str = "ask-notify-mcp";
pub const TRACKER_TOOLS: [&str; 3] = ["issue_create", "issue_list", "issue_close"];
pub const NOTIFY_TOOLS: [&str; 2] = ["notify_send", "notify_list"];
/// The triage flow, in the order its data dependencies force.
pub const FLOW: [&str; 6] = ["search", "git_log", "issue_create", "issue_list", "saveToFile", "notify_send"];
const LANE_W: usize = 15;

/// `ask-pipeline-mcp` → `pipeline`: lane titles and qualified tool names.
pub fn alias(server: &str) -> &str {
    let s = server.strip_prefix("ask-").unwrap_or(server);
    let s = s.strip_suffix("-mcp").unwrap_or(s);
    if s.is_empty() {
        "?"
    } else {
        s
    }
}

/// Which own server a tool belongs to — what the routing check compares
/// the actual route against.
pub fn owner(tool: &str) -> Option<&'static str> {
    match tool {
        "git_log" | "git_show" | "git_status" => Some(GIT),
        "search" | "summarize" | "saveToFile" => Some(toolchain::SERVER_NAME),
        t if TRACKER_TOOLS.contains(&t) => Some(TRACKER),
        t if NOTIFY_TOOLS.contains(&t) => Some(NOTIFY),
        _ => None,
    }
}

fn ask6() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(".ask6")
}

/// `~/.ask6/tracker.db`.
pub fn default_tracker_db() -> PathBuf {
    ask6().join("tracker.db")
}

/// `~/.ask6/notify/` (the outbox is `outbox.jsonl` inside).
pub fn default_notify_dir() -> PathBuf {
    ask6().join("notify")
}

fn req_str<'a>(args: &'a Value, key: &str) -> Res<&'a str> {
    args[key]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("`{key}` (string) is required"))
}

fn one_line(s: &str, max: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= max {
        return s;
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

fn spawn_on(port: u16, serve: impl FnOnce(TcpListener) + Send + 'static) -> Res<String> {
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| format!("bind 127.0.0.1:{port}: {e}"))?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    std::thread::spawn(move || serve(listener));
    Ok(format!("http://{addr}/mcp"))
}

// ---------------------------------------------------------------- tracker

pub struct Issue {
    pub id: i64,
    pub title: String,
    pub source: String,
    pub assignee: String,
    pub priority: String,
    pub status: String,
}

impl Issue {
    fn json(&self) -> Value {
        json!({"id": format!("T-{}", self.id), "title": self.title, "source": self.source,
               "assignee": self.assignee, "priority": self.priority, "status": self.status})
    }

    fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Issue> {
        Ok(Issue {
            id: r.get(0)?,
            title: r.get(1)?,
            source: r.get(2)?,
            assignee: r.get(3)?,
            priority: r.get(4)?,
            status: r.get(5)?,
        })
    }
}

const ISSUE_COLS: &str = "id, title, source, assignee, priority, status";

#[derive(Clone)]
pub struct Tracker {
    db: Arc<Mutex<Db>>,
    place: String,
    pub calls: CallLog,
    verbose: bool,
}

impl Tracker {
    pub fn open(path: &Path, verbose: bool) -> Res<Tracker> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let db = Db::open(path).map_err(|e| format!("tracker db {}: {e}", path.display()))?;
        Tracker::init(db, path.display().to_string(), verbose)
    }

    #[cfg(test)]
    pub fn in_memory() -> Res<Tracker> {
        Tracker::init(Db::open_in_memory().map_err(|e| e.to_string())?, ":memory:".into(), false)
    }

    fn init(db: Db, place: String, verbose: bool) -> Res<Tracker> {
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS issues (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 title TEXT NOT NULL,
                 source TEXT NOT NULL,
                 assignee TEXT NOT NULL DEFAULT '',
                 priority TEXT NOT NULL DEFAULT 'normal',
                 status TEXT NOT NULL DEFAULT 'open',
                 created INTEGER NOT NULL
             );",
        )
        .map_err(|e| format!("tracker db: {e}"))?;
        Ok(Tracker { db: Arc::new(Mutex::new(db)), place, calls: Arc::default(), verbose })
    }

    pub fn serve(&self, listener: TcpListener) {
        for stream in listener.incoming().flatten() {
            let r = mcp_server::handle_connection(stream, "mcp-tracker", self.verbose, &|m| self.handle(m));
            if let (Err(e), true) = (r, self.verbose) {
                eprintln!("[mcp-tracker] connection error: {e}");
            }
        }
    }

    pub fn spawn(&self, port: u16) -> Res<String> {
        let me = self.clone();
        spawn_on(port, move |l| me.serve(l))
    }

    pub fn handle(&self, msg: &Value) -> Option<Value> {
        let info = ServerInfo {
            name: TRACKER,
            instructions: format!(
                "The team's issue tracker (SQLite at {}): `issue_create` files a task, `issue_list` \
                 returns tasks as a Markdown table, `issue_close` closes one. Typical flow across the \
                 servers — triage of TODO markers: (1) `search` on the pipeline server, query `TODO:`, \
                 source files, case_sensitive true; (2) for every file among the hits `git_log` on the \
                 git server with `path` = that file and limit 1 — its author is the assignee; (3) \
                 `issue_create` for every hit with `source` = the hit's `path:line` exactly and that \
                 assignee; (4) `issue_list`; (5) `saveToFile` on the pipeline server with `content` = \
                 the issue_list output verbatim; (6) `notify_send` on the notify server with the saved \
                 file path. Never invent an assignee — take it from git.",
                self.place
            ),
        };
        mcp_server::dispatch(msg, &info, &tracker_specs(), &self.calls, &|name, args| self.call(name, args))
    }

    fn call(&self, name: &str, args: &Value) -> Res<(String, Value)> {
        match name {
            "issue_create" => self.create(args),
            "issue_list" => self.list(args),
            "issue_close" => self.close(args),
            _ => Err(format!("unknown tool: {name}")),
        }
    }

    fn db(&self) -> Res<std::sync::MutexGuard<'_, Db>> {
        self.db.lock().map_err(|e| e.to_string())
    }

    /// Issues with `status` (`open`, `closed` or `all`), oldest first.
    pub fn issues(&self, status: &str) -> Res<Vec<Issue>> {
        let db = self.db()?;
        let sql = format!("SELECT {ISSUE_COLS} FROM issues WHERE ?1 = 'all' OR status = ?1 ORDER BY id");
        let mut st = db.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = st.query_map(params![status], Issue::from_row).map_err(|e| e.to_string())?;
        rows.collect::<Result<_, _>>().map_err(|e| e.to_string())
    }

    fn create(&self, args: &Value) -> Res<(String, Value)> {
        let title = one_line(req_str(args, "title")?, 200);
        let source = req_str(args, "source")?;
        let assignee = args["assignee"].as_str().unwrap_or("").trim();
        let priority = match args["priority"].as_str().unwrap_or("normal") {
            p @ ("low" | "normal" | "high") => p,
            other => return Err(format!("`priority` must be low, normal or high, not {other:?}")),
        };
        let db = self.db()?;
        let existing = db
            .query_row(
                &format!("SELECT {ISSUE_COLS} FROM issues WHERE source = ?1 AND status = 'open'"),
                params![source],
                Issue::from_row,
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some(i) = existing {
            // Idempotent: re-running a triage must not duplicate tasks.
            return Ok((
                format!(
                    "T-{} уже открыта для {}: «{}» · @{} — новая не создана",
                    i.id,
                    i.source,
                    i.title,
                    or_dash(&i.assignee)
                ),
                json!({"id": format!("T-{}", i.id), "created": false, "issue": i.json()}),
            ));
        }
        db.execute(
            "INSERT INTO issues (title, source, assignee, priority, status, created) VALUES (?1, ?2, ?3, ?4, 'open', ?5)",
            params![title, source, assignee, priority, now()],
        )
        .map_err(|e| e.to_string())?;
        let issue = Issue {
            id: db.last_insert_rowid(),
            title,
            source: source.to_string(),
            assignee: assignee.to_string(),
            priority: priority.to_string(),
            status: "open".into(),
        };
        Ok((
            format!(
                "T-{} создана: «{}» · {} · @{} · {}",
                issue.id,
                issue.title,
                issue.source,
                or_dash(&issue.assignee),
                issue.priority
            ),
            json!({"id": format!("T-{}", issue.id), "created": true, "issue": issue.json()}),
        ))
    }

    fn list(&self, args: &Value) -> Res<(String, Value)> {
        let status = match args["status"].as_str().unwrap_or("open") {
            s @ ("open" | "closed" | "all") => s,
            other => return Err(format!("`status` must be open, closed or all, not {other:?}")),
        };
        let who = args["assignee"].as_str().map(str::trim).filter(|s| !s.is_empty());
        let issues: Vec<Issue> = self
            .issues(status)?
            .into_iter()
            .filter(|i| who.is_none_or(|w| i.assignee == w))
            .collect();
        let text = issue_table(status, &issues);
        let d = digest(&text);
        let list: Vec<Value> = issues.iter().map(Issue::json).collect();
        Ok((text, json!({"status": status, "count": issues.len(), "issues": list, "digest": d})))
    }

    fn close(&self, args: &Value) -> Res<(String, Value)> {
        let raw = match &args["id"] {
            Value::String(s) => s.trim().trim_start_matches("T-").to_string(),
            Value::Number(n) => n.to_string(),
            _ => return Err("`id` (like T-3) is required".into()),
        };
        let id: i64 = raw.parse().map_err(|_| format!("`id` must look like T-3, got {raw:?}"))?;
        let n = self
            .db()?
            .execute("UPDATE issues SET status = 'closed' WHERE id = ?1 AND status = 'open'", params![id])
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err(format!("no open issue T-{id}"));
        }
        Ok((format!("T-{id} закрыта"), json!({"id": format!("T-{id}"), "closed": true})))
    }
}

fn or_dash(s: &str) -> &str {
    if s.is_empty() {
        "—"
    } else {
        s
    }
}

/// `issue_list` output: a heading and a Markdown table — what `saveToFile`
/// is expected to store verbatim.
pub fn issue_table(status: &str, issues: &[Issue]) -> String {
    let mut out = format!("# Задачи ({status}): {}\n", issues.len());
    if issues.is_empty() {
        out.push_str("\n(нет задач)\n");
        return out;
    }
    out.push_str("\n| id | задача | где | исполнитель | приоритет |\n|---|---|---|---|---|\n");
    for i in issues {
        out.push_str(&format!(
            "| T-{} | {} | {} | {} | {} |\n",
            i.id,
            i.title.replace('|', "\\|"),
            i.source,
            or_dash(&i.assignee),
            i.priority
        ));
    }
    out
}

pub fn tracker_specs() -> Vec<Value> {
    vec![
        json!({
            "name": "issue_create",
            "description": "File a task in the team's issue tracker. Idempotent per `source`: if an open task for the same place exists, it is returned instead of a duplicate. Returns the id (T-N).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "What has to be done, one line."},
                    "source": {"type": "string", "description": "Where it comes from, `path:line` exactly as `search` returned it."},
                    "assignee": {"type": "string", "description": "Who does it — the author from git_log of that file."},
                    "priority": {"type": "string", "enum": ["low", "normal", "high"], "description": "Default normal."},
                },
                "required": ["title", "source"],
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "issue_list",
            "description": "Tasks of the tracker as a Markdown table (id, task, where, assignee, priority). Save or send this text verbatim.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "status": {"type": "string", "enum": ["open", "closed", "all"], "description": "Default open."},
                    "assignee": {"type": "string", "description": "Only this person's tasks."},
                },
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "issue_close",
            "description": "Close an open task by id.",
            "inputSchema": {
                "type": "object",
                "properties": {"id": {"type": "string", "description": "Task id like T-3."}},
                "required": ["id"],
                "additionalProperties": false,
            },
        }),
    ]
}

// ---------------------------------------------------------------- notify

#[derive(Clone)]
pub struct Notify {
    outbox: PathBuf,
    write: Arc<Mutex<()>>,
    pub calls: CallLog,
    verbose: bool,
}

impl Notify {
    pub fn new(dir: &Path, verbose: bool) -> Res<Notify> {
        std::fs::create_dir_all(dir).map_err(|e| format!("notify dir {}: {e}", dir.display()))?;
        let dir = dir.canonicalize().map_err(|e| e.to_string())?;
        Ok(Notify { outbox: dir.join("outbox.jsonl"), write: Arc::default(), calls: Arc::default(), verbose })
    }

    pub fn outbox(&self) -> &Path {
        &self.outbox
    }

    pub fn serve(&self, listener: TcpListener) {
        for stream in listener.incoming().flatten() {
            let r = mcp_server::handle_connection(stream, "mcp-notify", self.verbose, &|m| self.handle(m));
            if let (Err(e), true) = (r, self.verbose) {
                eprintln!("[mcp-notify] connection error: {e}");
            }
        }
    }

    pub fn spawn(&self, port: u16) -> Res<String> {
        let me = self.clone();
        spawn_on(port, move |l| me.serve(l))
    }

    pub fn handle(&self, msg: &Value) -> Option<Value> {
        let info = ServerInfo {
            name: NOTIFY,
            instructions: format!(
                "Team notifications: `notify_send` posts a message to a channel (team, dev, …); \
                 messages land in the outbox {} that the team reads — there is no other delivery. \
                 Use it as the last step of a flow to tell people where the result is: put the \
                 saved file path and/or task ids into the text.",
                self.outbox.display()
            ),
        };
        mcp_server::dispatch(msg, &info, &notify_specs(), &self.calls, &|name, args| self.call(name, args))
    }

    fn call(&self, name: &str, args: &Value) -> Res<(String, Value)> {
        match name {
            "notify_send" => self.send(args),
            "notify_list" => self.list(args),
            _ => Err(format!("unknown tool: {name}")),
        }
    }

    /// Every message in the outbox, oldest first.
    pub fn messages(&self) -> Res<Vec<Value>> {
        match std::fs::read_to_string(&self.outbox) {
            Ok(t) => Ok(t.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(format!("{}: {e}", self.outbox.display())),
        }
    }

    fn send(&self, args: &Value) -> Res<(String, Value)> {
        let channel = req_str(args, "channel")?.trim_start_matches('#');
        if channel.len() > 32 || !channel.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(format!("`channel` must be a plain name like team, got {channel:?}"));
        }
        let text = req_str(args, "text")?;
        if text.chars().count() > 4000 {
            return Err("`text` is longer than 4000 characters".into());
        }
        let _guard = self.write.lock().map_err(|e| e.to_string())?;
        let id = format!("m{}", self.messages()?.len() + 1);
        let at = now();
        let line = json!({"id": id, "at": at, "channel": channel, "text": text});
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.outbox)
            .map_err(|e| format!("{}: {e}", self.outbox.display()))?;
        writeln!(f, "{line}").map_err(|e| e.to_string())?;
        let d = digest(text);
        Ok((
            format!("[{id}] → #{channel}: {} ({} симв, #{})", one_line(text, 80), text.chars().count(), short(&d)),
            json!({"id": id, "channel": channel, "text": text, "at": at, "digest": d,
                   "outbox": self.outbox.display().to_string()}),
        ))
    }

    fn list(&self, args: &Value) -> Res<(String, Value)> {
        let channel = args["channel"].as_str().map(|c| c.trim().trim_start_matches('#')).filter(|c| !c.is_empty());
        let limit = match &args["limit"] {
            Value::Null => 10,
            v => v.as_u64().filter(|n| (1..=50).contains(n)).ok_or("`limit` must be an integer from 1 to 50")? as usize,
        };
        let all: Vec<Value> = self
            .messages()?
            .into_iter()
            .filter(|m| channel.is_none_or(|c| m["channel"] == c))
            .collect();
        let tail = &all[all.len().saturating_sub(limit)..];
        let text = if tail.is_empty() {
            "(сообщений нет)".to_string()
        } else {
            tail.iter()
                .map(|m| {
                    format!(
                        "[{}] {} #{}: {}",
                        m["id"].as_str().unwrap_or("?"),
                        fmt_time(m["at"].as_i64().unwrap_or(0)),
                        m["channel"].as_str().unwrap_or("?"),
                        m["text"].as_str().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        Ok((text, json!({"messages": tail})))
    }
}

pub fn notify_specs() -> Vec<Value> {
    vec![
        json!({
            "name": "notify_send",
            "description": "Post a message to a team channel (the team's outbox). Returns the message id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "channel": {"type": "string", "description": "Channel name like team or dev."},
                    "text": {"type": "string", "description": "Message text; include file paths / task ids the reader needs."},
                },
                "required": ["channel", "text"],
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "notify_list",
            "description": "Latest messages of the outbox, optionally of one channel.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "channel": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 50, "description": "Default 10."},
                },
                "additionalProperties": false,
            },
        }),
    ]
}

/// The chat's four servers over `root` (a git repository): git, pipeline
/// (search under `root`, files into `~/.ask6/pipeline/`), tracker
/// (`~/.ask6/tracker.db`) and notify (`~/.ask6/notify/`).
pub fn local_toolbox(root: &Path) -> Res<Toolbox> {
    let mut tb = Toolbox::local_git(root)?;
    tb.merge(Toolbox::local_pipeline(root, &toolchain::default_out())?);
    tb.merge(Toolbox::local_tracker(&default_tracker_db())?);
    tb.merge(Toolbox::local_notify(&default_notify_dir())?);
    Ok(tb)
}

// ---------------------------------------------------------------- the flow

pub struct TriageRequest {
    pub query: String,
    pub limit: usize,
    pub filename: String,
    pub channel: String,
}

impl TriageRequest {
    pub fn new(query: Option<&str>, filename: Option<&str>) -> TriageRequest {
        TriageRequest {
            query: query.map(str::trim).filter(|q| !q.is_empty()).unwrap_or("TODO:").to_string(),
            limit: 10,
            filename: filename.map(String::from).unwrap_or_else(|| "triage.md".into()),
            channel: "team".into(),
        }
    }
}

pub struct TriageReport {
    pub steps: Vec<ToolStep>,
    pub checks: Vec<Check>,
    pub path: String,
    pub table: String,
}

impl TriageReport {
    pub fn ok(&self) -> bool {
        self.checks.iter().all(|c| c.ok) && coverage(&self.steps).1
    }
}

/// One line of the audit: ✓ or ✗ and why.
pub struct Check {
    pub ok: bool,
    pub text: String,
}

impl Check {
    pub fn line(&self) -> String {
        format!("{} {}", if self.ok { "✓" } else { "✗" }, self.text)
    }
}

/// Title of the task for a hit: what follows the marker, comment
/// punctuation trimmed.
fn todo_title(text: &str, marker: &str) -> String {
    let t = text.split_once(marker).map(|(_, t)| t).unwrap_or(text);
    let t = t.trim().trim_start_matches([':', '-', ' ']).trim();
    one_line(if t.is_empty() { text } else { t }, 120)
}

fn file_of(source: &str) -> &str {
    source.rsplit_once(':').map(|(f, _)| f).unwrap_or(source)
}

fn same_path(a: &str, b: &str) -> bool {
    a.trim().trim_start_matches("./") == b.trim().trim_start_matches("./")
}

/// `path:line` of every hit of a `search` step.
fn hits_of(step: &ToolStep) -> Vec<String> {
    step.structured["hits"]
        .as_array()
        .map(|a| a.iter().filter_map(|h| h["where"].as_str().map(String::from)).collect())
        .unwrap_or_default()
}

fn author_of(step: &ToolStep) -> Option<&str> {
    step.structured["commits"][0]["author"].as_str()
}

fn call(
    caller: &mut dyn ToolCaller,
    lanes: &[String],
    steps: &mut Vec<ToolStep>,
    emit: &mut dyn FnMut(String),
    tool: &str,
    args: Value,
) -> usize {
    let (server, name) = caller.resolve(tool);
    let (result, is_error, structured) = match caller.call_tool(tool, args.clone()) {
        Ok(r) => (r.text, r.is_error, r.structured),
        Err(e) => (e, true, Value::Null),
    };
    let step = ToolStep { name, server, args, result, is_error, structured };
    emit(lane_row(lanes, steps.len() + 1, &step));
    steps.push(step);
    steps.len() - 1
}

/// The automatic triage: every step a `tools/call` through `caller` (the
/// chat's [`Toolbox`], so each goes to its own server), every input taken
/// from an earlier output. `emit` gets the lane picture row by row, then the
/// audit.
pub fn run_triage(
    caller: &mut dyn ToolCaller,
    req: &TriageRequest,
    lanes: &[String],
    emit: &mut dyn FnMut(String),
) -> Res<TriageReport> {
    emit(format!(
        "оркестрация: search → git_log → issue_create → issue_list → saveToFile → notify_send · «{}» · файл {} · канал #{}",
        req.query, req.filename, req.channel
    ));
    emit(lane_header(lanes));
    let mut steps = Vec::new();
    let found = call(
        caller,
        lanes,
        &mut steps,
        emit,
        "search",
        json!({"query": req.query, "source": "files", "limit": req.limit, "case_sensitive": true}),
    );
    if steps[found].is_error {
        return Err(format!("search: {}", steps[found].result));
    }
    let hits: Vec<(String, String)> = steps[found].structured["hits"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|h| (h["where"].as_str().unwrap_or("").to_string(), h["text"].as_str().unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default();
    if hits.is_empty() {
        emit("итог: search ничего не нашёл — заводить нечего, флоу остановлен на шаге 1".into());
        return Err(format!("no «{}» markers found", req.query));
    }
    let mut authors: BTreeMap<String, String> = BTreeMap::new();
    for (place, _) in &hits {
        let file = file_of(place).to_string();
        if authors.contains_key(&file) {
            continue;
        }
        let i = call(caller, lanes, &mut steps, emit, "git_log", json!({"path": file, "limit": 1}));
        authors.insert(file, author_of(&steps[i]).unwrap_or("").to_string());
    }
    for (place, text) in &hits {
        call(
            caller,
            lanes,
            &mut steps,
            emit,
            "issue_create",
            json!({"title": todo_title(text, &req.query), "source": place, "assignee": authors[file_of(place)]}),
        );
    }
    let listed = call(caller, lanes, &mut steps, emit, "issue_list", json!({"status": "open"}));
    let table = steps[listed].result.clone();
    let saved = call(
        caller,
        lanes,
        &mut steps,
        emit,
        "saveToFile",
        json!({"filename": req.filename, "content": table}),
    );
    let path = steps[saved].structured["path"].as_str().unwrap_or("").to_string();
    let issues = steps[listed].structured["count"].as_u64().unwrap_or(0);
    call(
        caller,
        lanes,
        &mut steps,
        emit,
        "notify_send",
        json!({"channel": req.channel, "text": format!("Триаж «{}»: {} меток, открытых задач {issues}. Отчёт: {path}", req.query, hits.len())}),
    );
    let checks = check_flow(&steps);
    for l in audit_lines(&steps, &checks) {
        emit(l);
    }
    Ok(TriageReport { steps, checks, path, table })
}

/// Audit of a flow — the automatic one or one the model assembled. Routing
/// first (each call went to the server that owns the tool), then one line
/// per step whose input must come from an earlier step's output. A step
/// fed from nowhere, or from a step that came *later*, is ✗ — that is what
/// "wrong order" means here, checked on data, not on names.
pub fn check_flow(steps: &[ToolStep]) -> Vec<Check> {
    let mut out = Vec::new();
    let routed: Vec<&ToolStep> = steps.iter().filter(|s| owner(&s.name).is_some()).collect();
    let wrong: Vec<String> = routed
        .iter()
        .filter(|s| owner(&s.name) != Some(s.server.as_str()))
        .map(|s| format!("{} ушёл на {} вместо {}", s.name, or_dash(&s.server), owner(&s.name).unwrap_or("?")))
        .collect();
    if !routed.is_empty() {
        out.push(Check {
            ok: wrong.is_empty(),
            text: if wrong.is_empty() {
                format!("маршрутизация: {0}/{0} вызовов попали на сервер-владелец инструмента", routed.len())
            } else {
                format!("маршрутизация: {}", wrong.join("; "))
            },
        });
    }
    for (i, s) in steps.iter().enumerate() {
        let n = i + 1;
        let before = &steps[..i];
        let find = |pred: &dyn Fn(&ToolStep) -> bool| before.iter().rposition(|p| !p.is_error && pred(p));
        if s.is_error {
            out.push(Check { ok: false, text: format!("шаг {n} {}: ошибка — {}", s.name, one_line(&s.result, 80)) });
            continue;
        }
        match s.name.as_str() {
            "git_log" => {
                let Some(path) = s.args["path"].as_str() else { continue };
                let from = find(&|p| p.name == "search" && hits_of(p).iter().any(|h| same_path(file_of(h), path)));
                out.push(Check {
                    ok: from.is_some(),
                    text: match from {
                        Some(k) => format!("шаг {n} git_log {path}: файл из выдачи search (шаг {})", k + 1),
                        None => format!("шаг {n} git_log {path}: такого файла нет в выдаче предыдущего search"),
                    },
                });
            }
            "issue_create" => {
                let source = s.args["source"].as_str().unwrap_or("");
                let assignee = s.args["assignee"].as_str().unwrap_or("").trim();
                let id = s.structured["id"].as_str().unwrap_or("?");
                let from = find(&|p| p.name == "search" && hits_of(p).iter().any(|h| h == source));
                let git = find(&|p| {
                    p.name == "git_log" && p.args["path"].as_str().is_some_and(|q| same_path(q, file_of(source)))
                });
                let author = git.and_then(|k| author_of(&steps[k]));
                let src = match from {
                    Some(k) => format!("источник {source} из search (шаг {})", k + 1),
                    None => format!("источник «{source}» не из выдачи предыдущего search"),
                };
                let who = match (git, author) {
                    (Some(k), Some(a)) if a == assignee => format!("исполнитель {a} = автор файла по git_log (шаг {})", k + 1),
                    (Some(k), a) => format!(
                        "исполнитель «{}», а git_log (шаг {}) говорит «{}»",
                        assignee,
                        k + 1,
                        a.unwrap_or("нет коммитов")
                    ),
                    (None, _) => format!("исполнитель «{assignee}» не подтверждён: до этого не было git_log по {}", file_of(source)),
                };
                out.push(Check {
                    ok: from.is_some() && author.is_some_and(|a| a == assignee),
                    text: format!("шаг {n} issue_create {id}: {src}; {who}"),
                });
            }
            "issue_list" => {
                let created = before.iter().filter(|p| p.name == "issue_create").count();
                let late = steps[i + 1..].iter().any(|p| p.name == "issue_create");
                if created == 0 && !late {
                    continue;
                }
                out.push(Check {
                    ok: !late,
                    text: if late {
                        format!("шаг {n} issue_list: вызван раньше, чем заведены все задачи")
                    } else {
                        format!("шаг {n} issue_list: после всех issue_create ({created})")
                    },
                });
            }
            "summarize" | "saveToFile" => {
                let got = s.structured["input"]["digest"].as_str().unwrap_or("");
                let from = find(&|p| {
                    p.structured["digest"].as_str() == Some(got) || digest(split_header(&p.result).1) == got
                });
                let via = s.structured["input"]["via"].as_str().unwrap_or("?");
                out.push(Check {
                    ok: from.is_some(),
                    text: match from {
                        Some(k) => format!(
                            "шаг {n} {} получил выход {} (шаг {}) без искажений (#{}, через {via})",
                            s.name,
                            steps[k].name,
                            k + 1,
                            short(got)
                        ),
                        None => format!(
                            "шаг {n} {}: вход #{} не совпадает побайтно ни с одним выходом предыдущих шагов — текст изменён при передаче",
                            s.name,
                            short(got)
                        ),
                    },
                });
            }
            "notify_send" => {
                let text = s.args["text"].as_str().unwrap_or("");
                let from = find(&|p| {
                    p.name == "saveToFile"
                        && p.structured["path"].as_str().is_some_and(|path| {
                            let name = Path::new(path).file_name().and_then(|f| f.to_str()).unwrap_or(path);
                            text.contains(path) || text.contains(name)
                        })
                });
                let ids = before.iter().any(|p| p.name == "issue_create");
                if from.is_some() || before.iter().any(|p| p.name == "saveToFile") || ids {
                    out.push(Check {
                        ok: from.is_some(),
                        text: match from {
                            Some(k) => format!("шаг {n} notify_send: в сообщении файл, сохранённый на шаге {}", k + 1),
                            None => format!("шаг {n} notify_send: в сообщении нет пути сохранённого отчёта"),
                        },
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// Which steps of the triage flow ran (successfully): `(line, all six)`.
pub fn coverage(steps: &[ToolStep]) -> (String, bool) {
    let marks: Vec<String> = FLOW
        .iter()
        .map(|t| {
            let n = steps.iter().filter(|s| s.name == *t && !s.is_error).count();
            match n {
                0 => format!("{t} —"),
                1 => t.to_string(),
                n => format!("{t}×{n}"),
            }
        })
        .collect();
    let all = FLOW.iter().all(|t| steps.iter().any(|s| s.name == *t && !s.is_error));
    (format!("флоу триажа: {} ({})", marks.join(" · "), if all { "все 6 шагов" } else { "не полный" }), all)
}

/// Servers in the order the flow visited them, consecutive repeats folded.
pub fn server_path(steps: &[ToolStep]) -> String {
    let mut path: Vec<&str> = Vec::new();
    for s in steps {
        let a = alias(&s.server);
        if path.last() != Some(&a) {
            path.push(a);
        }
    }
    path.join(" → ")
}

/// Whether a turn's calls are worth the lane picture: more than one server,
/// or anything of the tracker / notify.
pub fn is_orchestrated(steps: &[ToolStep]) -> bool {
    let mut servers: Vec<&str> = steps.iter().map(|s| s.server.as_str()).collect();
    servers.sort_unstable();
    servers.dedup();
    servers.len() > 1 || steps.iter().any(|s| s.server == TRACKER || s.server == NOTIFY)
}

/// The audit block under the lane picture.
pub fn audit_lines(steps: &[ToolStep], checks: &[Check]) -> Vec<String> {
    let mut lines = vec![format!("путь по серверам: {}", server_path(steps))];
    // Покрытие флоу триажа — только когда ход и был триажем (нашёл и завёл),
    // иначе «не полный» у «закрой T-2» только путает.
    if ["search", "issue_create"].iter().all(|t| steps.iter().any(|s| s.name == *t)) {
        lines.push(coverage(steps).0);
    }
    lines.extend(checks.iter().map(Check::line));
    lines
}

fn pad(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n >= w {
        format!("{s} ")
    } else {
        format!("{s}{}", " ".repeat(w - n))
    }
}

pub fn lane_header(lanes: &[String]) -> String {
    let mut row = "  #  ".to_string();
    for l in lanes {
        row.push_str(&pad(l, LANE_W));
    }
    row.push_str("  результат");
    row.trim_end().to_string()
}

/// One call as a row: `●` in its server's lane, `┆` in the others, and a
/// short tool-aware summary of the result on the right.
pub fn lane_row(lanes: &[String], n: usize, step: &ToolStep) -> String {
    let lane = alias(&step.server);
    let mut row = format!("{n:>3}  ");
    for l in lanes {
        if l == lane {
            row.push_str(&pad(&format!("● {}", step.name), LANE_W));
        } else {
            row.push_str(&pad("┆", LANE_W));
        }
    }
    if !lanes.iter().any(|l| l == lane) {
        row.push_str(&format!("[{lane}] ● {} ", step.name));
    }
    format!("{row} {}", brief(step))
}

fn brief(step: &ToolStep) -> String {
    if step.is_error {
        return format!("ERROR {}", one_line(&step.result, 50));
    }
    let s = &step.structured;
    match step.name.as_str() {
        "git_log" => format!(
            "{} → {}",
            step.args["path"].as_str().unwrap_or("весь репозиторий"),
            author_of(step).unwrap_or("нет коммитов")
        ),
        "search" => format!(
            "«{}»: {} совп. [{}]",
            s["query"].as_str().unwrap_or("?"),
            s["count"],
            s["id"].as_str().unwrap_or("?")
        ),
        "issue_create" => format!(
            "{} {} → @{}{}",
            s["id"].as_str().unwrap_or("?"),
            s["issue"]["source"].as_str().unwrap_or("?"),
            or_dash(s["issue"]["assignee"].as_str().unwrap_or("")),
            if s["created"] == false { " (уже была)" } else { "" }
        ),
        "issue_list" => format!("{} задач, {} симв #{}", s["count"], step.result.chars().count(), short(s["digest"].as_str().unwrap_or(""))),
        "saveToFile" => format!(
            "{} ({} байт) #{}",
            s["path"].as_str().map(|p| Path::new(p).file_name().and_then(|f| f.to_str()).unwrap_or(p)).unwrap_or("?"),
            s["bytes"],
            short(s["digest"].as_str().unwrap_or(""))
        ),
        "notify_send" => format!("[{}] #{}", s["id"].as_str().unwrap_or("?"), s["channel"].as_str().unwrap_or("?")),
        _ => one_line(split_header(&step.result).0.unwrap_or(&step.result), 50),
    }
}

/// The whole picture of a finished flow: title, lanes, one row per call.
pub fn render(steps: &[ToolStep], lanes: &[String]) -> Vec<String> {
    let mut used: Vec<&str> = steps.iter().map(|s| alias(&s.server)).collect();
    used.sort_unstable();
    used.dedup();
    let mut lines = vec![
        format!("оркестрация MCP · серверов: {} · вызовов: {}", used.len(), steps.len()),
        lane_header(lanes),
    ];
    lines.extend(steps.iter().enumerate().map(|(i, s)| lane_row(lanes, i + 1, s)));
    lines
}

// ---------------------------------------------------------------- fixture

pub struct Fixture {
    /// `(path:line, the author who last touched that file)` of every marker.
    pub todos: Vec<(String, String)>,
    /// Author of HEAD — what a `git_log` without `path` would suggest.
    pub head_author: String,
    pub codename: String,
}

/// A small repository with three `TODO:` markers in three files by two
/// authors, plus a later commit by a third one (so the *latest* commit is
/// not the right assignee) and a decoy `todo` in lowercase. `tag` makes the
/// names unguessable in the proof; empty for the demo.
pub fn fixture_repo(dir: &Path, tag: &str) -> Res<Fixture> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let who = |name: &str| format!("{name}{tag} <{}@example.com>", name.to_lowercase());
    let (ada, boris, carol) = (format!("Ada{tag}"), format!("Boris{tag}"), format!("Carol{tag}"));
    let codename = if tag.is_empty() { String::new() } else { format!("KV-{tag}") };
    let parser_todo = if tag.is_empty() {
        "// TODO: обработать пустой ввод".to_string()
    } else {
        format!("// TODO: {codename} обработать пустой ввод")
    };
    mcp_server::git(dir, &["init", "-q", "-b", "main"])?;
    let commits: Vec<(&str, String, &str, String)> = vec![
        ("README.md", "# demo\n".into(), "initial commit", who("Carol")),
        ("src/lib.rs", "pub mod todo; // список дел, не метка\n".into(), "add lib", who("Carol")),
        (
            "src/parser.rs",
            format!("pub fn parse(s: &str) -> Vec<&str> {{\n    s.split(',').collect()\n}}\n{parser_todo}\n"),
            "add parser",
            who("Ada"),
        ),
        ("src/net.rs", "pub fn fetch() {}\n// TODO: повторять запрос при таймауте\n".into(), "add net", who("Boris")),
        ("docs/plan.md", "# План\n\n- TODO: описать формат отчёта\n".into(), "add plan", who("Boris")),
        ("README.md", "# demo\n\nПроект для проверки оркестрации MCP.\n".into(), "docs: readme", who("Carol")),
    ];
    for (file, body, msg, author) in &commits {
        let path = dir.join(file);
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, body).map_err(|e| e.to_string())?;
        mcp_server::git(dir, &["add", file])?;
        mcp_server::git(
            dir,
            &[
                "-c", "user.name=fixture", "-c", "user.email=fixture@example.com",
                "-c", "commit.gpgsign=false",
                "commit", "-q", "--author", author, "-m", msg,
            ],
        )?;
    }
    Ok(Fixture {
        todos: vec![
            ("docs/plan.md:3".into(), boris.clone()),
            ("src/net.rs:2".into(), boris),
            ("src/parser.rs:4".into(), ada),
        ],
        head_author: carol,
        codename,
    })
}

/// `ask --orchestra-demo DIR`: the fixture as a playground for the chat.
pub fn demo(dir: &Path) -> Res<Fixture> {
    if dir.exists() && std::fs::read_dir(dir).map(|mut d| d.next().is_some()).unwrap_or(true) {
        return Err(format!("{} already exists and is not empty", dir.display()));
    }
    fixture_repo(dir, "")
}

// ---------------------------------------------------------------- verify

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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

/// Four servers on four ports and a toolbox over them — the chat's setup.
struct Rig {
    git: mcp_server::Server,
    pipeline: toolchain::Server,
    tracker: Tracker,
    notify: Notify,
    tb: Toolbox,
}

impl Rig {
    fn new(repo: &Path, base: &Path, tag: &str) -> Res<Rig> {
        let git = mcp_server::Server::new(repo, false)?;
        let pipeline = toolchain::Server::new(repo, &base.join("out"), false)?;
        let tracker = Tracker::open(&base.join(format!("tracker-{tag}.db")), false)?;
        let notify = Notify::new(&base.join(format!("notify-{tag}")), false)?;
        let mut tb = Toolbox::connect(&git.spawn(0)?, "git".into())?;
        tb.merge(Toolbox::connect(&pipeline.spawn(0)?, "pipeline".into())?);
        tb.merge(Toolbox::connect(&tracker.spawn(0)?, "tracker".into())?);
        tb.merge(Toolbox::connect(&notify.spawn(0)?, "notify".into())?);
        Ok(Rig { git, pipeline, tracker, notify, tb })
    }

    /// What each server itself logged, by server name — independent of
    /// what the client believes it routed.
    fn logs(&self) -> Res<Vec<(&'static str, Vec<String>)>> {
        let names = |log: &CallLog| -> Res<Vec<String>> {
            Ok(log
                .lock()
                .map_err(|e| e.to_string())?
                .iter()
                .map(|c| c.split_whitespace().next().unwrap_or("").to_string())
                .collect())
        };
        Ok(vec![
            (GIT, names(&self.git.calls)?),
            (toolchain::SERVER_NAME, names(&self.pipeline.calls)?),
            (TRACKER, names(&self.tracker.calls)?),
            (NOTIFY, names(&self.notify.calls)?),
        ])
    }

    /// Every server's own log equals the steps the client routed to it, in order.
    fn routing_matches(&self, steps: &[ToolStep]) -> Res<bool> {
        let mut ok = true;
        let mut lines = Vec::new();
        for (server, logged) in self.logs()? {
            let routed: Vec<String> = steps.iter().filter(|s| s.server == server).map(|s| s.name.clone()).collect();
            let same = routed == logged;
            ok &= same;
            lines.push(format!(
                "    {} журнал {:<9} {}",
                if same { "=" } else { "≠" },
                alias(server),
                if logged.is_empty() { "—".into() } else { logged.join(", ") }
            ));
        }
        println!("[{}] журнал каждого сервера = вызовы, которые клиент отправил на него:", mark(ok));
        for l in lines {
            println!("{l}");
        }
        Ok(ok)
    }

    /// The tracker holds exactly the fixture's markers, each on the right person.
    fn tracker_matches(&self, fx: &Fixture) -> Res<bool> {
        let got: Vec<(String, String)> = self
            .tracker
            .issues("open")?
            .into_iter()
            .map(|i| (i.source, i.assignee))
            .collect();
        let mut want = fx.todos.clone();
        want.sort();
        let mut sorted = got.clone();
        sorted.sort();
        let ok = sorted == want;
        println!(
            "[{}] в трекере {} задач(и), исполнители из git: {}",
            mark(ok),
            got.len(),
            got.iter().map(|(s, a)| format!("{s}→{a}")).collect::<Vec<_>>().join(", ")
        );
        Ok(ok)
    }

    fn clear_logs(&self) -> Res<()> {
        for log in [&self.git.calls, &self.pipeline.calls, &self.tracker.calls, &self.notify.calls] {
            log.lock().map_err(|e| e.to_string())?.clear();
        }
        Ok(())
    }
}

fn file_and_notice(steps: &[ToolStep], rig: &Rig, filename: &str, codename: &str) -> Res<bool> {
    let table = steps
        .iter()
        .rev()
        .find(|s| s.name == "issue_list" && !s.is_error)
        .map(|s| s.result.clone());
    let path = rig.pipeline.out().join(filename);
    let on_disk = std::fs::read_to_string(&path).ok();
    let file_ok = on_disk.is_some() && on_disk == table;
    println!(
        "[{}] {} существует и побайтно равен выходу issue_list",
        mark(file_ok),
        path.display()
    );
    let carried = on_disk.as_deref().is_some_and(|t| t.contains(codename));
    println!("[{}] кодовое имя {codename} из файла репозитория доехало до отчёта", mark(carried));
    let last = rig.notify.messages()?.pop();
    let notice = last.as_ref().and_then(|m| m["text"].as_str()).is_some_and(|t| t.contains(filename));
    println!(
        "[{}] в outbox сообщение со ссылкой на отчёт: {}",
        mark(notice),
        last.map(|m| format!("#{} {}", m["channel"].as_str().unwrap_or("?"), m["text"].as_str().unwrap_or("")))
            .unwrap_or_else(|| "—".into())
    );
    Ok(file_ok && carried && notice)
}

/// `ask --verify-orchestra offline|live|all`.
///
/// A fixture repository with three `TODO:` markers whose authors have
/// random names, and four real servers on four ports.
///
/// * offline — [`run_triage`] through the toolbox: Confirmed only if the
///   audit is all ✓ with all six steps, every server's *own* call log
///   equals what was routed to it, the tracker holds exactly the three
///   markers each on the author git names for that file, the report on disk
///   equals `issue_list`, and the outbox message names it. Four controls
///   must fail: a step moved before its producer, an assignee taken from
///   HEAD instead of the file's history, a call tagged with the wrong
///   server, and a real name collision that has to be routed by qualified
///   name to the right one of two servers.
/// * live — the *model* gets one sentence that names no tool and no
///   server; the same checks, on a fresh tracker and outbox.
pub fn verify(which: &str, settings: &Settings) -> Res<bool> {
    let (offline, live) = match which {
        "offline" => (true, false),
        "live" => (false, true),
        "all" | "" => (true, true),
        other => return Err(format!("--verify-orchestra: unknown {other:?} (offline|live|all)")),
    };
    let code = stamp();
    let base = TempDir(std::env::temp_dir().join(format!("ask-orchestra-proof-{code}")));
    let repo = base.0.join("repo");
    let fx = fixture_repo(&repo, &code)?;
    println!(
        "репозиторий-фикстура: {} — метки TODO: {}; HEAD от {}",
        repo.display(),
        fx.todos.iter().map(|(s, a)| format!("{s} ({a})")).collect::<Vec<_>>().join(", "),
        fx.head_author
    );
    let mut all_ok = true;

    if offline {
        println!("\n== offline: автоматический флоу через 4 сервера ==");
        let rig = Rig::new(&repo, &base.0, "offline")?;
        for s in &rig.tb.servers {
            println!("  {} — {}", s.conn.server_name, s.tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", "));
        }
        let mut tb = rig.tb;
        let lanes = tb.lanes();
        let file = format!("triage-{code}.md");
        let rep = run_triage(&mut tb, &TriageRequest::new(None, Some(&file)), &lanes, &mut |l| println!("{l}"))?;
        let rig = Rig { tb, ..rig };
        println!("[{}] аудит флоу: все шаги ✓, покрыты все 6", mark(rep.ok()));
        let routed = rig.routing_matches(&rep.steps)?;
        let tracked = rig.tracker_matches(&fx)?;
        let delivered = file_and_notice(&rep.steps, &rig, &file, &fx.codename)?;

        // Controls: each tampered flow must be caught.
        let clone = |s: &ToolStep| ToolStep {
            name: s.name.clone(),
            server: s.server.clone(),
            args: s.args.clone(),
            result: s.result.clone(),
            is_error: s.is_error,
            structured: s.structured.clone(),
        };
        let caught = |steps: &[ToolStep]| check_flow(steps).iter().any(|c| !c.ok);
        let mut reordered: Vec<ToolStep> = rep.steps.iter().map(clone).collect();
        let first_issue = reordered.iter().position(|s| s.name == "issue_create").unwrap_or(0);
        let moved = reordered.remove(first_issue);
        reordered.insert(0, moved);
        let c1 = caught(&reordered);
        println!("[{}] контроль 1: issue_create перед search — порядок пойман аудитом", mark(c1));
        let mut wrong_who: Vec<ToolStep> = rep.steps.iter().map(clone).collect();
        if let Some(s) = wrong_who.iter_mut().find(|s| s.name == "issue_create") {
            s.args["assignee"] = json!(fx.head_author);
        }
        let c2 = caught(&wrong_who);
        println!(
            "[{}] контроль 2: исполнитель = автор HEAD ({}), а не файла — пойман",
            mark(c2),
            fx.head_author
        );
        let mut misrouted: Vec<ToolStep> = rep.steps.iter().map(clone).collect();
        if let Some(s) = misrouted.iter_mut().find(|s| s.name == "issue_create") {
            s.server = NOTIFY.into();
        }
        let c3 = caught(&misrouted);
        println!("[{}] контроль 3: issue_create, записанный на notify — пойман", mark(c3));
        let c4 = collision_control(&repo, &base.0)?;
        let ok = rep.ok() && routed && tracked && delivered && c1 && c2 && c3 && c4;
        println!("offline: {}", if ok { "Confirmed" } else { "Flat" });
        all_ok &= ok;
    }

    if live {
        println!("\n== live: флоу собирает модель {} ==", settings.model);
        let rig = Rig::new(&repo, &base.0, "live")?;
        rig.clear_logs()?;
        let file = format!("triage-live-{code}.md");
        let question = format!(
            "Разбери TODO-метки в проекте: на каждую заведи задачу в трекере на того, кто последним менял \
             этот файл. Потом сохрани список открытых задач в файл {file} и сообщи команде в канал team, \
             где лежит отчёт."
        );
        println!("вопрос: {question}");
        let ep = Endpoint::for_model(&settings.model)?;
        let mut tb = rig.tb;
        let lanes = tb.lanes();
        let system = format!(
            "You are an agent with MCP tools. Answer in the language of the question.\n\n{}",
            mcp_agent::chat_note(&tb)
        );
        let functions = tb.functions.clone();
        println!("{}", lane_header(&lanes));
        let mut n = 0;
        let (outcome, steps, rounds) = mcp_agent::tool_loop(
            &ep,
            settings,
            &system,
            vec![json!({"role": "user", "content": question})],
            &mut tb,
            &functions,
            &mut |s| {
                n += 1;
                println!("{}", lane_row(&lanes, n, s));
            },
        )?;
        let rig = Rig { tb, ..rig };
        let checks = check_flow(&steps);
        for l in audit_lines(&steps, &checks) {
            println!("    {l}");
        }
        let (_, full) = coverage(&steps);
        let audited = full && checks.iter().all(|c| c.ok);
        println!("[{}] аудит флоу: все шаги ✓, покрыты все 6", mark(audited));
        let extra: Vec<&str> = steps.iter().filter(|s| !FLOW.contains(&s.name.as_str())).map(|s| s.name.as_str()).collect();
        println!(
            "    выбор инструментов: {} вызовов, вне флоу: {}",
            steps.len(),
            if extra.is_empty() { "нет".into() } else { extra.join(", ") }
        );
        let routed = rig.routing_matches(&steps)?;
        let tracked = rig.tracker_matches(&fx)?;
        let delivered = file_and_notice(&steps, &rig, &file, &fx.codename)?;
        println!("    ответ модели: {}", one_line(outcome.text(), 300));
        println!(
            "    раундов {} [{}]",
            rounds.len(),
            rounds
                .iter()
                .map(|r| format!("{} ({}→{})", r.finish_reason, r.prompt_tokens, r.completion_tokens))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let ok = audited && routed && tracked && delivered;
        println!("live: {}", if ok { "Confirmed" } else { "Flat" });
        all_ok &= ok;
    }
    Ok(all_ok)
}

/// Two pipeline servers at once share `search`: both must be exposed under
/// qualified names, the bare name must be refused, and each qualified call
/// must land on its own server — seen in the servers' own logs and data.
fn collision_control(repo: &Path, base: &Path) -> Res<bool> {
    let other = base.join("other");
    std::fs::create_dir_all(&other).map_err(|e| e.to_string())?;
    std::fs::write(other.join("x.md"), "TODO: чужая метка\n").map_err(|e| e.to_string())?;
    let a = toolchain::Server::new(repo, &base.join("out-a"), false)?;
    let b = toolchain::Server::new(&other, &base.join("out-b"), false)?;
    let mut tb = Toolbox::connect(&a.spawn(0)?, "a".into())?;
    // `merge` replaces a server with the same name; here we want both.
    let second = Toolbox::connect(&b.spawn(0)?, "b".into())?;
    tb.servers.extend(second.servers);
    tb.rebuild();
    let bare = tb.call_tool("search", json!({"query": "TODO:"}));
    let hit_b = tb.call_tool("pipeline2__search", json!({"query": "TODO:", "case_sensitive": true}))?;
    let (la, lb) = (a.calls.lock().map_err(|e| e.to_string())?.len(), b.calls.lock().map_err(|e| e.to_string())?.len());
    let ok = bare.is_err()
        && tb.tool_names().contains("pipeline__search")
        && tb.tool_names().contains("pipeline2__search")
        && hit_b.text.contains("x.md:1")
        && (la, lb) == (0, 1);
    println!(
        "[{}] контроль 4: два сервера с `search` → pipeline__search / pipeline2__search; голое имя отклонено ({}); \
         pipeline2__search ушёл только на второй (журналы {la}/{lb})",
        mark(ok),
        bare.err().map(|e| one_line(&e, 60)).unwrap_or_else(|| "НЕ отклонено".into())
    );
    Ok(ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::Connection;

    fn call(handle: &dyn Fn(&Value) -> Option<Value>, name: &str, args: Value) -> Value {
        handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                       "params": {"name": name, "arguments": args}}))
            .unwrap()["result"]
            .clone()
    }

    #[test]
    fn tracker_creates_lists_dedupes_and_closes() {
        let t = Tracker::in_memory().unwrap();
        let h = |m: &Value| t.handle(m);
        let r = call(&h, "issue_create", json!({"title": "fix | pipe", "source": "a.rs:1", "assignee": "Ada"}));
        assert_eq!(r["isError"], false);
        assert_eq!(r["structuredContent"]["id"], "T-1");
        let again = call(&h, "issue_create", json!({"title": "other", "source": "a.rs:1"}));
        assert_eq!(again["structuredContent"]["created"], false);
        assert_eq!(again["structuredContent"]["id"], "T-1");
        let list = call(&h, "issue_list", json!({}));
        let text = list["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("# Задачи (open): 1") && text.contains("| T-1 | fix \\| pipe | a.rs:1 | Ada | normal |"), "{text}");
        assert_eq!(list["structuredContent"]["digest"], digest(text));
        assert_eq!(call(&h, "issue_close", json!({"id": "T-1"}))["isError"], false);
        assert_eq!(call(&h, "issue_close", json!({"id": "T-1"}))["isError"], true);
        assert_eq!(call(&h, "issue_list", json!({}))["structuredContent"]["count"], 0);
        assert_eq!(call(&h, "issue_create", json!({"title": "x"}))["isError"], true);
        assert_eq!(call(&h, "issue_create", json!({"title": "x", "source": "b", "priority": "urgent"}))["isError"], true);
    }

    #[test]
    fn notify_appends_to_outbox() {
        let dir = std::env::temp_dir().join(format!("ask-notify-test-{}", std::process::id()));
        let _g = TempDir(dir.clone());
        let n = Notify::new(&dir, false).unwrap();
        let h = |m: &Value| n.handle(m);
        let r = call(&h, "notify_send", json!({"channel": "#team", "text": "отчёт в triage.md"}));
        assert_eq!(r["structuredContent"]["id"], "m1");
        assert_eq!(r["structuredContent"]["channel"], "team");
        call(&h, "notify_send", json!({"channel": "dev", "text": "второе"}));
        assert_eq!(n.messages().unwrap().len(), 2);
        let l = call(&h, "notify_list", json!({"channel": "team"}));
        assert!(l["content"][0]["text"].as_str().unwrap().contains("отчёт в triage.md"));
        assert_eq!(call(&h, "notify_send", json!({"channel": "a b", "text": "x"}))["isError"], true);
    }

    #[test]
    fn triage_over_four_servers_is_routed_ordered_and_audited() {
        let base = TempDir(std::env::temp_dir().join(format!("ask-orchestra-test-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&base.0);
        let repo = base.0.join("repo");
        let fx = fixture_repo(&repo, "T").unwrap();
        let rig = Rig::new(&repo, &base.0, "t").unwrap();
        let mut tb = rig.tb;
        assert_eq!(tb.lanes(), ["git", "pipeline", "tracker", "notify"]);
        let lanes = tb.lanes();
        let mut seen = Vec::new();
        let rep = run_triage(&mut tb, &TriageRequest::new(None, Some("t.md")), &lanes, &mut |l| seen.push(l)).unwrap();
        let rig = Rig { tb, ..rig };
        assert!(rep.ok(), "{seen:#?}");
        let names: Vec<&str> = rep.steps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            ["search", "git_log", "git_log", "git_log", "issue_create", "issue_create", "issue_create",
             "issue_list", "saveToFile", "notify_send"]
        );
        assert_eq!(server_path(&rep.steps), "pipeline → git → tracker → pipeline → notify");
        assert!(rig.routing_matches(&rep.steps).unwrap());
        assert!(rig.tracker_matches(&fx).unwrap());
        assert!(file_and_notice(&rep.steps, &rig, "t.md", &fx.codename).unwrap());
        // The picture: the ● of issue_create sits under the `tracker` title.
        let row = seen.iter().find(|l| l.contains("● issue_create")).unwrap();
        let col = row.chars().position(|c| c == '●').unwrap();
        assert!(seen[1].chars().skip(col).collect::<String>().starts_with("tracker"), "{seen:#?}");
        // Idempotent second run: no duplicates, every issue_create says «уже была».
        let mut tb = rig.tb;
        let again = run_triage(&mut tb, &TriageRequest::new(None, Some("t.md")), &[], &mut |_| {}).unwrap();
        assert!(again.ok());
        assert_eq!(rig.tracker.issues("all").unwrap().len(), 3);
        assert!(again.steps.iter().filter(|s| s.name == "issue_create").all(|s| s.structured["created"] == false));
    }

    #[test]
    fn audit_catches_order_assignee_route_and_tampering() {
        let base = TempDir(std::env::temp_dir().join(format!("ask-orchestra-audit-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&base.0);
        let repo = base.0.join("repo");
        let fx = fixture_repo(&repo, "A").unwrap();
        let rig = Rig::new(&repo, &base.0, "a").unwrap();
        let mut tb = rig.tb;
        let rep = run_triage(&mut tb, &TriageRequest::new(None, Some("a.md")), &[], &mut |_| {}).unwrap();
        let bad = |f: &dyn Fn(&mut Vec<ToolStep>)| {
            let mut steps: Vec<ToolStep> = rep
                .steps
                .iter()
                .map(|s| ToolStep {
                    name: s.name.clone(),
                    server: s.server.clone(),
                    args: s.args.clone(),
                    result: s.result.clone(),
                    is_error: s.is_error,
                    structured: s.structured.clone(),
                })
                .collect();
            f(&mut steps);
            check_flow(&steps).iter().filter(|c| !c.ok).count()
        };
        assert_eq!(bad(&|_| {}), 0);
        assert!(bad(&|s| s.swap(0, 4)) > 0, "issue_create before search");
        assert!(bad(&|s| s[4].args["assignee"] = json!(fx.head_author)) > 0, "assignee from HEAD");
        assert!(bad(&|s| s[1].server = TRACKER.into()) > 0, "git_log routed to tracker");
        assert!(bad(&|s| { let l = s.remove(7); s.insert(5, l); }) > 0, "issue_list before the last issue_create");
        assert!(bad(&|s| s[9].args["text"] = json!("готово")) > 0, "message without the report");
        // saveToFile fed with an edited table: its input digest no longer
        // matches any earlier output.
        let edited = tb
            .call_tool("saveToFile", json!({"filename": "b.md", "content": format!("{}\nP.S.", rep.table)}))
            .unwrap();
        assert!(bad(&|s| s[8].structured = edited.structured.clone()) > 0, "edited hand-off");
    }

    #[test]
    fn toolbox_qualifies_colliding_names_and_routes_each() {
        let base = TempDir(std::env::temp_dir().join(format!("ask-orchestra-coll-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&base.0);
        let repo = base.0.join("repo");
        fixture_repo(&repo, "C").unwrap();
        assert!(collision_control(&repo, &base.0).unwrap());
        // Unique names stay bare.
        let t = Tracker::in_memory().unwrap();
        let mut tb = Toolbox::connect(&t.spawn(0).unwrap(), "t".into()).unwrap();
        assert_eq!(tb.tool_names(), "issue_create, issue_list, issue_close");
        assert_eq!(tb.resolve("issue_list"), (TRACKER.to_string(), "issue_list".to_string()));
        let c = Connection::connect(&t.spawn(0).unwrap()).unwrap();
        assert_eq!(c.server_name, TRACKER);
        assert!(tb.call_tool("nope", json!({})).is_err());
    }

    #[test]
    fn lane_rows_put_the_dot_under_the_server() {
        let lanes: Vec<String> = ["git", "pipeline", "tracker"].iter().map(|s| s.to_string()).collect();
        let step = ToolStep {
            name: "git_log".into(),
            server: GIT.into(),
            args: json!({"path": "a.rs"}),
            result: String::new(),
            is_error: false,
            structured: json!({"commits": [{"author": "Ada"}]}),
        };
        let row = lane_row(&lanes, 2, &step);
        assert!(row.starts_with("  2  ● git_log"), "{row:?}");
        assert!(row.ends_with("a.rs → Ada"), "{row:?}");
        let head = lane_header(&lanes);
        let dot = row.chars().position(|c| c == '●').unwrap();
        assert_eq!(head.chars().nth(dot), Some('g'));
        assert_eq!(todo_title("// TODO: повторять запрос", "TODO:"), "повторять запрос");
        assert_eq!(alias("ask-pipeline-mcp"), "pipeline");
        assert_eq!(alias("DeepWiki"), "DeepWiki");
    }
}
