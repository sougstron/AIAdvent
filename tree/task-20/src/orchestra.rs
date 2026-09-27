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
/// The commit review flow: which commit, what it changed, who reviews each file.
pub const REVIEW_FLOW: [&str; 6] = ["git_log", "git_show", "issue_create", "issue_list", "saveToFile", "notify_send"];

/// A named flow: what the coverage line counts and what "complete" means.
pub struct Flow {
    pub name: &'static str,
    pub tools: &'static [&'static str],
}

pub const TRIAGE: Flow = Flow { name: "триаж", tools: &FLOW };
pub const REVIEW: Flow = Flow { name: "ревью", tools: &REVIEW_FLOW };
/// How many changed files one review walks at most.
const REVIEW_MAX_FILES: usize = 12;
/// Lines changed from which a file's review is `high` priority.
const REVIEW_BIG: u64 = 300;
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
    /// Which run filed it (`triage:TODO:`, `review:68b003e`) — so one run's
    /// report lists its own tasks, not everything open in the tracker.
    pub label: String,
}

impl Issue {
    fn json(&self) -> Value {
        json!({"id": format!("T-{}", self.id), "title": self.title, "source": self.source,
               "assignee": self.assignee, "priority": self.priority, "status": self.status, "label": self.label})
    }

    fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Issue> {
        Ok(Issue {
            id: r.get(0)?,
            title: r.get(1)?,
            source: r.get(2)?,
            assignee: r.get(3)?,
            priority: r.get(4)?,
            status: r.get(5)?,
            label: r.get(6)?,
        })
    }
}

const ISSUE_COLS: &str = "id, title, source, assignee, priority, status, label";

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
        // Trackers made before labels existed get the column; on newer ones
        // this fails with "duplicate column", which is the state we want.
        let _ = db.execute("ALTER TABLE issues ADD COLUMN label TEXT NOT NULL DEFAULT ''", []);
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
                 file path. Second flow — review of a commit: (1) `git_log` (limit 1, `path` = the \
                 folder if the user named one) gives the commit, or the user names it; (2) `git_show` \
                 with that hash lists the changed files and the author; (3) for every non-binary \
                 changed file `git_log` with `path` = that file — the reviewer is the most recent \
                 author of that file other than the commit's author (the author only if nobody else \
                 ever touched it); (4) `issue_create` per file with `source` = `path@<7-char hash>`, \
                 that reviewer and `label` = `review:<7-char hash>`; (5) `issue_list` with that \
                 `label`; (6) \
                 `saveToFile` with the issue_list output verbatim; (7) `notify_send` (channel dev) \
                 with the saved path. Never invent an assignee — take it from git.",
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
        let label = one_line(args["label"].as_str().unwrap_or(""), 60);
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
            "INSERT INTO issues (title, source, assignee, priority, status, created, label) VALUES (?1, ?2, ?3, ?4, 'open', ?5, ?6)",
            params![title, source, assignee, priority, now(), label],
        )
        .map_err(|e| e.to_string())?;
        let issue = Issue {
            id: db.last_insert_rowid(),
            title,
            source: source.to_string(),
            assignee: assignee.to_string(),
            priority: priority.to_string(),
            status: "open".into(),
            label,
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
        let from = args["source"].as_str().map(str::trim).filter(|s| !s.is_empty());
        let label = args["label"].as_str().map(str::trim).filter(|s| !s.is_empty());
        let issues: Vec<Issue> = self
            .issues(status)?
            .into_iter()
            .filter(|i| who.is_none_or(|w| i.assignee == w))
            .filter(|i| from.is_none_or(|f| i.source.contains(f)))
            .filter(|i| label.is_none_or(|l| i.label == l))
            .collect();
        let text = issue_table(&label.map_or(status.to_string(), |l| format!("{status}, {l}")), &issues);
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
                    "source": {"type": "string", "description": "Where it comes from: `path:line` exactly as `search` returned it, or `path@<7-char commit hash>` for a review of that file in that commit."},
                    "assignee": {"type": "string", "description": "Who does it — taken from git_log of that file, never invented."},
                    "priority": {"type": "string", "enum": ["low", "normal", "high"], "description": "Default normal."},
                    "label": {"type": "string", "description": "Which run files it, e.g. `review:68b003e` or `triage:TODO:` — issue_list can then list just this run."},
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
                    "source": {"type": "string", "description": "Only tasks whose source contains this text, e.g. `@68b003e` for one commit's review or `src/` for one folder."},
                    "label": {"type": "string", "description": "Only tasks with exactly this label (as given to issue_create)."},
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

/// The chat's four servers over the repository `root` is in (its top level,
/// so a run from `target/release` still sees the whole project): git,
/// pipeline (search there, files into `~/.ask6/pipeline/`), tracker
/// (`~/.ask6/tracker.db`) and notify (`~/.ask6/notify/`).
pub fn local_toolbox(root: &Path) -> Res<Toolbox> {
    let root = &mcp_server::toplevel(root);
    let mut tb = Toolbox::local_git(root)?;
    tb.merge(Toolbox::local_pipeline(root, &toolchain::default_out())?);
    tb.merge(Toolbox::local_tracker(&default_tracker_db())?);
    tb.merge(Toolbox::local_notify(&default_notify_dir())?);
    Ok(tb)
}

// ---------------------------------------------------------------- the flow

pub struct TriageRequest {
    pub query: String,
    /// Folder or file to search in, relative to the repository root.
    pub path: Option<String>,
    pub limit: usize,
    pub filename: String,
    pub channel: String,
}

impl TriageRequest {
    pub fn new(query: Option<&str>, filename: Option<&str>) -> TriageRequest {
        TriageRequest {
            query: query.map(str::trim).filter(|q| !q.is_empty()).unwrap_or("TODO:").to_string(),
            path: None,
            limit: 10,
            filename: filename.map(String::from).unwrap_or_else(|| "triage.md".into()),
            channel: "team".into(),
        }
    }
}

/// What an automatic flow (triage or review) leaves behind.
pub struct TriageReport {
    pub flow: &'static Flow,
    pub steps: Vec<ToolStep>,
    pub checks: Vec<Check>,
    pub path: String,
    pub table: String,
}

impl TriageReport {
    pub fn ok(&self) -> bool {
        self.checks.iter().all(|c| c.ok) && coverage(&self.steps, self.flow).1
    }
}

/// `/triage` and `/review` arguments: `<main> [in <path>] [> <file>]`.
pub fn split_flow_args(rest: &str) -> (String, Option<String>, Option<String>) {
    let (rest, file) = match rest.rsplit_once('>') {
        Some((q, f)) => (q, Some(f.trim().to_string()).filter(|f| !f.is_empty())),
        None => (rest, None),
    };
    let words: Vec<&str> = rest.split_whitespace().collect();
    match words.iter().rposition(|w| *w == "in" || *w == "в") {
        Some(k) if k + 1 < words.len() => (words[..k].join(" "), Some(words[k + 1..].join(" ")), file),
        _ => (words.join(" "), None, file),
    }
}

/// One line of the audit: ✓ or ✗ and why. `soft` marks a failed check
/// that is a fact, not necessarily a fault — a file holding the model's own
/// text instead of a tool's output — shown as ≈; it still is not ✓.
pub struct Check {
    pub ok: bool,
    pub soft: bool,
    pub text: String,
}

impl Check {
    pub fn line(&self) -> String {
        let mark = match (self.ok, self.soft) {
            (true, _) => "✓",
            (false, true) => "≈",
            (false, false) => "✗",
        };
        format!("{mark} {}", self.text)
    }
}

/// A marker like `TODO:` or `FIXME:` (ends with a colon) — a note left in
/// a comment, not a piece of code like `panic!`.
fn is_marker(query: &str) -> bool {
    query.ends_with(':')
}

/// Whether a `marker` hit is a real note: the line is a comment (or a list
/// item) that *starts* with the marker — `// TODO: x`, `# FIXME: y`,
/// `- TODO: z` — not a sentence or a string that merely mentions it.
fn marker_in_comment(text: &str, marker: &str) -> bool {
    let t = text.trim_start();
    ["<!--", "//", "/*", "--", "#", "*", ";", "-"].iter().any(|open| {
        t.strip_prefix(open)
            .map(|r| r.trim_start_matches(['/', '!', '*', '-', '#']).trim_start())
            .is_some_and(|r| r.starts_with(marker))
    })
}

/// Title of the task for a hit: for a marker what follows it, comment
/// punctuation trimmed; for anything else (`panic!`) the line itself.
fn todo_title(text: &str, marker: &str) -> String {
    if !is_marker(marker) {
        return one_line(text, 120);
    }
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

/// Why a `git_log{path}` came back empty, in words: the git server's
/// `path_state` (task 20 fix) — a folder never committed on this branch is
/// not "no commits" by accident, and the flows must say so.
fn no_commits(step: &ToolStep) -> String {
    let st = &step.structured["path_state"];
    let branch = st["branch"].as_str().unwrap_or("?");
    match st["state"].as_str() {
        Some("untracked") => format!("не закоммичен на ветке {branch}"),
        Some("ignored") => "в .gitignore — в git его нет".into(),
        Some("missing") => "такого пути нет".into(),
        _ => "нет коммитов".into(),
    }
}

/// The file has no history at all (untracked / ignored), so it has no author
/// either — an empty assignee is then the right answer, not a mistake.
fn never_committed(step: &ToolStep) -> bool {
    matches!(step.structured["path_state"]["state"].as_str(), Some("untracked" | "ignored"))
}

/// Who reviews a file that `author` changed, by the file's `git_log`: the
/// most recent *other* author, or `author` when nobody else ever touched
/// it. `None` when the log has no commits at all.
fn reviewer_of(log: &ToolStep, author: &str) -> Option<String> {
    let commits = log.structured["commits"].as_array().filter(|c| !c.is_empty())?;
    let other = commits.iter().filter_map(|c| c["author"].as_str()).find(|a| *a != author);
    Some(other.unwrap_or(author).to_string())
}

/// `path@abc1234` → `("path", "abc1234")`: the source of a review issue.
fn review_source(source: &str) -> Option<(&str, &str)> {
    let (file, hash) = source.rsplit_once('@')?;
    (hash.len() >= 7 && hash.chars().all(|c| c.is_ascii_hexdigit()) && !file.is_empty()).then_some((file, hash))
}

/// `T-6` named in `text` as itself, not as the start of `T-60`.
fn mentions_id(text: &str, id: &str) -> bool {
    text.match_indices(id).any(|(k, _)| !text[k + id.len()..].starts_with(|c: char| c.is_ascii_digit()))
}

fn short_hash(hash: &str) -> &str {
    &hash[..hash.len().min(7)]
}

/// Changed files of a `git_show` step: `(path, added, deleted, binary)`.
fn files_of(step: &ToolStep) -> Vec<(String, u64, u64, bool)> {
    step.structured["files"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|f| {
                    Some((
                        f["path"].as_str()?.to_string(),
                        f["added"].as_u64().unwrap_or(0),
                        f["deleted"].as_u64().unwrap_or(0),
                        f["binary"].as_bool().unwrap_or(false),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
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
        {
            // A marker is filtered to comments below, so ask for more lines.
            let limit = if is_marker(&req.query) { 50 } else { req.limit };
            let mut a = json!({"query": req.query, "source": "files", "limit": limit, "case_sensitive": true});
            if let Some(p) = &req.path {
                a["path"] = json!(p);
            }
            a
        },
    );
    if steps[found].is_error {
        return Err(format!("search: {}", steps[found].result));
    }
    let found_all: Vec<(String, String)> = steps[found].structured["hits"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|h| (h["where"].as_str().unwrap_or("").to_string(), h["text"].as_str().unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default();
    let hits: Vec<(String, String)> = found_all
        .iter()
        .filter(|(_, t)| !is_marker(&req.query) || marker_in_comment(t, &req.query))
        .take(req.limit)
        .cloned()
        .collect();
    if hits.is_empty() {
        let mentions = found_all.len();
        emit(format!(
            "итог: меток «{}» нет{}{} — заводить нечего, флоу остановлен на шаге 1. \
             На реальной истории репозитория работает /review [коммит] [in папка]; \
             по коду — /triage <что искать> in <папка>, например /triage panic! in tree/task-20/src",
            req.query,
            req.path.as_deref().map(|p| format!(" в {p}")).unwrap_or_default(),
            if mentions > 0 { format!(" (строк с «{}»: {mentions}, но ни одна не метка в комментарии)", req.query) } else { String::new() }
        ));
        return Err(format!("меток «{}» нет — попробуйте /review или /triage <что искать> in <папка>", req.query));
    }
    let label = format!("triage:{}", req.query);
    let mut authors: BTreeMap<String, String> = BTreeMap::new();
    for (place, _) in &hits {
        let file = file_of(place).to_string();
        if authors.contains_key(&file) {
            continue;
        }
        let i = call(caller, lanes, &mut steps, emit, "git_log", json!({"path": file, "limit": 1}));
        authors.insert(file, author_of(&steps[i]).unwrap_or("").to_string());
    }
    let orphans: Vec<&ToolStep> = steps.iter().filter(|s| s.name == "git_log" && never_committed(s)).collect();
    if let Some(first) = orphans.first() {
        emit(format!(
            "внимание: у {} из {} файлов нет истории в git ({}) — авторов нет, задачи по ним заводятся без \
             исполнителя. Закоммитьте их, и исполнителем станет автор последнего коммита файла",
            orphans.len(),
            authors.len(),
            no_commits(first)
        ));
    }
    for (place, text) in &hits {
        call(
            caller,
            lanes,
            &mut steps,
            emit,
            "issue_create",
            json!({"title": todo_title(text, &req.query), "source": place, "assignee": authors[file_of(place)], "label": label}),
        );
    }
    let listed = call(caller, lanes, &mut steps, emit, "issue_list", json!({"status": "open", "label": label}));
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
    Ok(TriageReport { flow: &TRIAGE, steps, checks, path, table })
}

pub struct ReviewRequest {
    /// Commit to review; `None` = the latest one (touching `path`, if given).
    pub rev: Option<String>,
    /// Folder the latest commit is looked up in, relative to the repository.
    pub path: Option<String>,
    pub filename: Option<String>,
    pub channel: String,
}

impl ReviewRequest {
    pub fn new(rev: Option<&str>, path: Option<&str>, filename: Option<&str>) -> ReviewRequest {
        let some = |s: Option<&str>| s.map(str::trim).filter(|s| !s.is_empty()).map(String::from);
        ReviewRequest { rev: some(rev), path: some(path), filename: some(filename), channel: "dev".into() }
    }
}

/// The automatic review of one commit across the four servers, every input
/// taken from an earlier output:
///
/// ```text
/// git.git_log{path, limit 1} → git.git_show{hash} → git.git_log{file} ×files
///   → tracker.issue_create{file@hash, reviewer} ×files → tracker.issue_list{@hash}
///   → pipeline.saveToFile{issue_list} → notify.notify_send{path}
/// ```
///
/// The reviewer of a file is the latest other person in that file's
/// history — so the author never reviews themselves unless nobody else is
/// there.
pub fn run_review(
    caller: &mut dyn ToolCaller,
    req: &ReviewRequest,
    lanes: &[String],
    emit: &mut dyn FnMut(String),
) -> Res<TriageReport> {
    emit(format!(
        "оркестрация «ревью»: git_log → git_show → git_log×файлы → issue_create×файлы → issue_list → saveToFile → notify_send · коммит {} · канал #{}",
        match (&req.rev, &req.path) {
            (Some(r), _) => r.clone(),
            (None, Some(p)) => format!("последний в {p}"),
            (None, None) => "последний".into(),
        },
        req.channel
    ));
    emit(lane_header(lanes));
    let mut steps = Vec::new();
    let rev = match &req.rev {
        Some(r) => r.clone(),
        None => {
            let mut a = json!({"limit": 1});
            if let Some(p) = &req.path {
                a["path"] = json!(p);
            }
            let i = call(caller, lanes, &mut steps, emit, "git_log", a);
            match steps[i].structured["commits"][0]["hash"].as_str() {
                Some(h) if !steps[i].is_error => h.to_string(),
                _ if never_committed(&steps[i]) => {
                    let st = &steps[i].structured["path_state"];
                    let p = req.path.as_deref().unwrap_or("");
                    emit(format!(
                        "итог: {p} {} ({} файлов вне git) — у этих файлов нет ни одного коммита, ревьюить нечего. \
                         Ревью смотрит закоммиченную историю: закоммитьте папку (git add {p} && git commit) и \
                         повторите /review in {p}; или /review без in — последний коммит всего репозитория; \
                         по незакоммиченному коду работает /triage <что искать> in {p}",
                        no_commits(&steps[i]),
                        st["untracked_files"].as_u64().unwrap_or(0)
                    ));
                    return Err(format!("{p}: {}, истории нет", no_commits(&steps[i])));
                }
                _ => return Err(format!("git_log: нет коммитов — {}", one_line(&steps[i].result, 120))),
            }
        }
    };
    let shown = call(caller, lanes, &mut steps, emit, "git_show", json!({"rev": rev}));
    if steps[shown].is_error {
        return Err(format!("git_show {rev}: {}", one_line(&steps[shown].result, 120)));
    }
    let hash = steps[shown].structured["hash"].as_str().unwrap_or("").to_string();
    let short = short_hash(&hash).to_string();
    let author = steps[shown].structured["author"].as_str().unwrap_or("").to_string();
    let subject = steps[shown].structured["subject"].as_str().unwrap_or("").to_string();
    let files: Vec<(String, u64, u64, bool)> = files_of(&steps[shown]).into_iter().filter(|f| !f.3).collect();
    if files.is_empty() {
        emit(format!("итог: в коммите {short} нет текстовых файлов — ревьюить нечего"));
        return Err(format!("{short}: нет изменённых текстовых файлов"));
    }
    let skipped = files.len().saturating_sub(REVIEW_MAX_FILES);
    let mut reviewers: Vec<String> = Vec::new();
    for (file, ..) in files.iter().take(REVIEW_MAX_FILES) {
        let i = call(caller, lanes, &mut steps, emit, "git_log", json!({"path": file, "limit": 10}));
        reviewers.push(reviewer_of(&steps[i], &author).unwrap_or_else(|| author.clone()));
    }
    let topic = one_line(&subject, 40);
    for ((file, added, deleted, _), who) in files.iter().take(REVIEW_MAX_FILES).zip(&reviewers) {
        call(
            caller,
            lanes,
            &mut steps,
            emit,
            "issue_create",
            json!({
                "title": format!("Ревью {short} «{topic}»: {file} (+{added}/−{deleted})"),
                "source": format!("{file}@{short}"),
                "assignee": who,
                "priority": if added + deleted >= REVIEW_BIG { "high" } else { "normal" },
                "label": format!("review:{short}"),
            }),
        );
    }
    let listed = call(
        caller,
        lanes,
        &mut steps,
        emit,
        "issue_list",
        json!({"status": "open", "label": format!("review:{short}")}),
    );
    let table = steps[listed].result.clone();
    let filename = req.filename.clone().unwrap_or_else(|| format!("review-{short}.md"));
    let saved = call(caller, lanes, &mut steps, emit, "saveToFile", json!({"filename": filename, "content": table}));
    let path = steps[saved].structured["path"].as_str().unwrap_or("").to_string();
    let mut people: Vec<&str> = reviewers.iter().map(String::as_str).collect();
    people.sort_unstable();
    people.dedup();
    call(
        caller,
        lanes,
        &mut steps,
        emit,
        "notify_send",
        json!({"channel": req.channel, "text": format!(
            "Ревью {short} «{topic}» (автор {author}): {} файлов{} → ревьюеры {}. Чек-лист: {path}",
            reviewers.len(),
            if skipped > 0 { format!(" (ещё {skipped} не взяты)") } else { String::new() },
            people.join(", ")
        )}),
    );
    let checks = check_flow(&steps);
    for l in audit_lines(&steps, &checks) {
        emit(l);
    }
    Ok(TriageReport { flow: &REVIEW, steps, checks, path, table })
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
            soft: false,
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
            out.push(Check { soft: false, ok: false, text: format!("шаг {n} {}: ошибка — {}", s.name, one_line(&s.result, 80)) });
            continue;
        }
        match s.name.as_str() {
            "git_log" => {
                let Some(path) = s.args["path"].as_str() else { continue };
                // A path with no search / git_show before it came from the user.
                if !before.iter().any(|p| p.name == "search" || p.name == "git_show") {
                    continue;
                }
                let listed = |p: &ToolStep| files_of(p).iter().any(|f| same_path(&f.0, path));
                let from = find(&|p| {
                    (p.name == "search" && hits_of(p).iter().any(|h| same_path(file_of(h), path)))
                        || (p.name == "git_show" && listed(p))
                });
                out.push(Check {
                    soft: false,
                    ok: from.is_some(),
                    text: match from {
                        Some(k) if steps[k].name == "git_show" => {
                            format!("шаг {n} git_log {path}: файл из изменённых в git_show (шаг {})", k + 1)
                        }
                        Some(k) => format!("шаг {n} git_log {path}: файл из выдачи search (шаг {})", k + 1),
                        None => format!("шаг {n} git_log {path}: такого файла нет в выдаче предыдущего search / git_show"),
                    },
                });
            }
            "git_show" => {
                let rev = s.args["rev"].as_str().unwrap_or("").trim();
                // Only a hash can be traced; HEAD, a tag or a branch the user named.
                if rev.len() < 7 || !rev.chars().all(|c| c.is_ascii_hexdigit()) {
                    continue;
                }
                let from = find(&|p| {
                    p.name == "git_log"
                        && p.structured["commits"]
                            .as_array()
                            .is_some_and(|c| c.iter().any(|c| c["hash"].as_str().is_some_and(|h| h.starts_with(rev))))
                });
                if from.is_none() && !before.iter().any(|p| p.name == "git_log") {
                    continue;
                }
                out.push(Check {
                    soft: false,
                    ok: from.is_some(),
                    text: match from {
                        Some(k) => format!("шаг {n} git_show {}: коммит из выдачи git_log (шаг {})", short_hash(rev), k + 1),
                        None => format!("шаг {n} git_show {}: такого коммита нет в выдаче предыдущего git_log", short_hash(rev)),
                    },
                });
            }
            "issue_create" if review_source(s.args["source"].as_str().unwrap_or("")).is_some() => {
                let source = s.args["source"].as_str().unwrap_or("");
                let (file, hash) = review_source(source).unwrap_or_default();
                let assignee = s.args["assignee"].as_str().unwrap_or("").trim();
                let id = s.structured["id"].as_str().unwrap_or("?");
                // The commit is known either from git_show (it lists the file)
                // or from the file's own git_log (the commit is in its history).
                fn in_log<'a>(p: &'a ToolStep, hash: &str) -> Option<&'a str> {
                    p.structured["commits"]
                        .as_array()
                        .and_then(|c| c.iter().find(|c| c["hash"].as_str().is_some_and(|h| h.starts_with(hash))))
                        .and_then(|c| c["author"].as_str())
                }
                let shown = find(&|p| {
                    p.name == "git_show"
                        && p.structured["hash"].as_str().is_some_and(|h| h.starts_with(hash))
                        && files_of(p).iter().any(|f| same_path(&f.0, file))
                })
                .or_else(|| {
                    find(&|p| {
                        p.name == "git_log"
                            && p.args["path"].as_str().is_some_and(|q| same_path(q, file))
                            && in_log(p, hash).is_some()
                    })
                });
                let author = shown.and_then(|k| match steps[k].name.as_str() {
                    "git_show" => steps[k].structured["author"].as_str(),
                    _ => in_log(&steps[k], hash),
                });
                let log = find(&|p| p.name == "git_log" && p.args["path"].as_str().is_some_and(|q| same_path(q, file)));
                let want = match (log, author) {
                    (Some(k), Some(a)) => reviewer_of(&steps[k], a),
                    _ => None,
                };
                let src = match shown {
                    Some(k) => format!("файл {file} изменён в {hash} по {} (шаг {})", steps[k].name, k + 1),
                    None => format!("«{source}»: ни git_show, ни git_log до этого не связывают этот файл с этим коммитом"),
                };
                let who = match (log, want.as_deref()) {
                    (Some(k), Some(w)) if w == assignee => format!(
                        "ревьюер {w} по git_log файла (шаг {}){}",
                        k + 1,
                        if author == Some(w) { " — других авторов у файла нет" } else { ", не автор коммита" }
                    ),
                    (Some(k), w) => format!(
                        "ревьюер «{assignee}», а по git_log файла (шаг {}) должен быть «{}»",
                        k + 1,
                        w.unwrap_or("?")
                    ),
                    (None, _) => format!("ревьюер «{assignee}» не подтверждён: до этого не было git_log по {file}"),
                };
                out.push(Check {
                    soft: false,
                    ok: shown.is_some() && want.as_deref() == Some(assignee),
                    text: format!("шаг {n} issue_create {id}: {src}; {who}"),
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
                let orphan = git.is_some_and(|k| never_committed(&steps[k])) && author.is_none() && assignee.is_empty();
                let who = match (git, author) {
                    (Some(k), Some(a)) if a == assignee => format!("исполнитель {a} = автор файла по git_log (шаг {})", k + 1),
                    (Some(k), None) if orphan => format!(
                        "без исполнителя: файл {} по git_log (шаг {}), автора нет",
                        no_commits(&steps[k]),
                        k + 1
                    ),
                    (Some(k), a) => format!(
                        "исполнитель «{}», а git_log (шаг {}) говорит «{}»",
                        assignee,
                        k + 1,
                        a.unwrap_or("нет коммитов")
                    ),
                    (None, _) => format!("исполнитель «{assignee}» не подтверждён: до этого не было git_log по {}", file_of(source)),
                };
                out.push(Check {
                    soft: false,
                    ok: from.is_some() && (orphan || author.is_some_and(|a| a == assignee)),
                    text: format!("шаг {n} issue_create {id}: {src}; {who}"),
                });
            }
            "issue_list" => {
                let created = before.iter().filter(|p| p.name == "issue_create").count();
                let late = steps[i + 1..].iter().any(|p| p.name == "issue_create");
                if created == 0 && !late {
                    continue;
                }
                // A look at the tracker before any work ("покажи открытые задачи") is
                // fine as long as a later issue_list sees every task and this early,
                // incomplete list is not the one that ends up saved or summarized.
                let rest = &steps[i + 1..];
                let recheck = rest.iter().enumerate().any(|(j, p)| {
                    p.name == "issue_list" && !rest[j + 1..].iter().any(|q| q.name == "issue_create")
                });
                let mine = s.structured["digest"].as_str();
                let consumed = mine.is_some()
                    && rest.iter().any(|p| {
                        matches!(p.name.as_str(), "summarize" | "saveToFile")
                            && p.structured["input"]["digest"].as_str() == mine
                    });
                if created == 0 && recheck && !consumed {
                    out.push(Check {
                        soft: false,
                        ok: true,
                        text: format!("шаг {n} issue_list: просмотр трекера до работы; полный список — позже"),
                    });
                    continue;
                }
                out.push(Check {
                    soft: false,
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
                    soft: from.is_none(),
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
                            "шаг {n} {}: записан текст модели, а не выход инструмента — вход #{} побайтно не совпадает ни с одним выходом предыдущих шагов",
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
                if from.is_some() || before.iter().any(|p| p.name == "saveToFile") {
                    out.push(Check {
                        soft: false,
                        ok: from.is_some(),
                        text: match from {
                            Some(k) => format!("шаг {n} notify_send: в сообщении файл, сохранённый на шаге {}", k + 1),
                            None => format!("шаг {n} notify_send: в сообщении нет пути сохранённого отчёта"),
                        },
                    });
                    continue;
                }
                // No report saved: the message must name a task this turn
                // filed or closed — that is what it is about.
                let touched: Vec<String> = before
                    .iter()
                    .filter(|p| !p.is_error && (p.name == "issue_create" || p.name == "issue_close"))
                    .filter_map(|p| p.structured["id"].as_str().map(String::from))
                    .collect();
                if touched.is_empty() {
                    continue;
                }
                let named: Vec<&String> = touched.iter().filter(|id| mentions_id(text, id)).collect();
                out.push(Check {
                    soft: false,
                    ok: !named.is_empty(),
                    text: if named.is_empty() {
                        format!("шаг {n} notify_send: в сообщении нет ни одной из задач хода ({})", touched.join(", "))
                    } else {
                        format!(
                            "шаг {n} notify_send: в сообщении {} — задачи, заведённые/закрытые выше",
                            named.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                        )
                    },
                });
            }
            "issue_close" => {
                let Some(id) = s.structured["id"].as_str() else { continue };
                let listed = |p: &ToolStep| {
                    (p.name == "issue_list"
                        && p.structured["issues"].as_array().is_some_and(|a| a.iter().any(|i| i["id"] == id)))
                        || (p.name == "issue_create" && p.structured["id"] == id)
                };
                // An id with no tracker call before it came from the user.
                if !before.iter().any(|p| p.name == "issue_list" || p.name == "issue_create") {
                    continue;
                }
                let from = find(&listed);
                out.push(Check {
                    soft: false,
                    ok: from.is_some(),
                    text: match from {
                        Some(k) => format!("шаг {n} issue_close {id}: задача из выдачи {} (шаг {})", steps[k].name, k + 1),
                        None => format!("шаг {n} issue_close {id}: такой задачи не было в выдаче трекера выше"),
                    },
                });
            }
            _ => {}
        }
    }
    out
}

/// [`check_flow`] for a chat turn: an `issue_close` id the user named in
/// `request` ("закрой T-2") is theirs, not invented — even if the task was
/// created in an earlier turn and no tracker output of this turn shows it. The
/// close itself still has to succeed, so the tracker vouches that it existed.
pub fn check_turn(steps: &[ToolStep], request: &str) -> Vec<Check> {
    let mut out = check_flow(steps);
    for c in out.iter_mut().filter(|c| !c.ok) {
        let Some(n) = c.text.strip_prefix("шаг ").and_then(|t| t.split(' ').next()?.parse::<usize>().ok()) else {
            continue;
        };
        let s = &steps[n - 1];
        let Some(id) = s.structured["id"].as_str() else { continue };
        if s.name == "issue_close" && !s.is_error && mentions_id(request, id) {
            c.ok = true;
            c.text = format!("шаг {n} issue_close {id}: задачу назвал пользователь в запросе, трекер её закрыл");
        }
    }
    out
}

/// Which steps of `flow` ran (successfully): `(line, all of them)`.
pub fn coverage(steps: &[ToolStep], flow: &Flow) -> (String, bool) {
    let marks: Vec<String> = flow
        .tools
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
    let all = flow.tools.iter().all(|t| steps.iter().any(|s| s.name == *t && !s.is_error));
    (
        format!(
            "флоу «{}»: {} ({})",
            flow.name,
            marks.join(" · "),
            if all { format!("все {} шагов", flow.tools.len()) } else { "не полный".into() }
        ),
        all,
    )
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
    // Покрытие флоу — только когда ход и был этим флоу (нашёл / показал
    // коммит и завёл задачи), иначе «не полный» у «закрой T-2» только путает.
    let has = |t: &str| steps.iter().any(|s| s.name == t);
    let report = has("issue_list") || has("saveToFile");
    if has("search") && has("issue_create") && report {
        lines.push(coverage(steps, &TRIAGE).0);
    }
    if has("git_show") && has("issue_create") && report {
        lines.push(coverage(steps, &REVIEW).0);
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
        "git_log" => {
            let n = s["commits"].as_array().map_or(0, Vec::len);
            let head = s["commits"][0]["hash"].as_str().map(short_hash).unwrap_or("");
            match step.args["path"].as_str() {
                Some(p) if n > 1 => format!("{p}: {n} комм., последний {}", author_of(step).unwrap_or("?")),
                Some(p) => format!("{p} → {}", author_of(step).map(String::from).unwrap_or_else(|| no_commits(step))),
                None => format!("{head} {} ({n} комм.)", author_of(step).unwrap_or("нет коммитов")),
            }
        }
        "git_show" => {
            let files = files_of(step);
            format!(
                "{} {}: {} файлов (+{}/−{})",
                short_hash(s["hash"].as_str().unwrap_or("?")),
                s["author"].as_str().unwrap_or("?"),
                files.len(),
                files.iter().map(|f| f.1).sum::<u64>(),
                files.iter().map(|f| f.2).sum::<u64>()
            )
        }
        "search" => format!(
            "«{}»{}: {} совп. [{}]",
            s["query"].as_str().unwrap_or("?"),
            step.args["path"].as_str().map(|p| format!(" в {p}")).unwrap_or_default(),
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
    /// The latest commit touching `src/` — what `/review in src` reviews.
    pub review_hash: String,
    pub review_author: String,
    /// `(file, reviewer)` of that commit: the latest *other* author of each file.
    pub review: Vec<(String, String)>,
}

/// A small repository with three `TODO:` markers in three files by two
/// authors, plus a later commit by a third one (so the *latest* commit is
/// not the right assignee) and a decoy `todo` in lowercase. Before that
/// last commit Ada changes two files that Carol and Boris wrote — the
/// commit a review walks, where each file has a different reviewer and
/// neither is the author. `tag` makes the names unguessable in the proof;
/// empty for the demo.
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
    let net = "pub fn fetch() {}\n// TODO: повторять запрос при таймауте\n";
    mcp_server::git(dir, &["init", "-q", "-b", "main"])?;
    let one = |f: &'static str, body: String| vec![(f, body)];
    // (files with their new content, message, author) — one commit each.
    type Commit<'a> = (Vec<(&'a str, String)>, String, String);
    let commits: Vec<Commit> = vec![
        (one("README.md", "# demo\n".into()), "initial commit".into(), who("Carol")),
        (one("src/lib.rs", "pub mod todo; // список дел, не метка\n".into()), "add lib".into(), who("Carol")),
        (
            one("src/parser.rs", format!("pub fn parse(s: &str) -> Vec<&str> {{\n    s.split(',').collect()\n}}\n{parser_todo}\n")),
            "add parser".into(),
            who("Ada"),
        ),
        (one("src/net.rs", net.into()), "add net".into(), who("Boris")),
        (one("docs/plan.md", "# План\n\n- TODO: описать формат отчёта\n".into()), "add plan".into(), who("Boris")),
        (
            vec![
                ("src/net.rs", format!("{net}pub const TIMEOUT_MS: u64 = 5000;\n")),
                ("src/lib.rs", "pub mod todo; // список дел, не метка\npub mod net;\n".into()),
            ],
            format!("net: таймаут из конфига {codename}").trim_end().to_string(),
            who("Ada"),
        ),
        (
            one("README.md", "# demo\n\nПроект для проверки оркестрации MCP.\n".into()),
            "docs: readme".into(),
            who("Carol"),
        ),
    ];
    for (files, msg, author) in &commits {
        for (file, body) in files {
            let path = dir.join(file);
            if let Some(p) = path.parent() {
                std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
            }
            std::fs::write(&path, body).map_err(|e| e.to_string())?;
            mcp_server::git(dir, &["add", file])?;
        }
        mcp_server::git(
            dir,
            &[
                "-c", "user.name=fixture", "-c", "user.email=fixture@example.com",
                "-c", "commit.gpgsign=false",
                "commit", "-q", "--author", author, "-m", msg,
            ],
        )?;
    }
    let review_hash = mcp_server::git(dir, &["rev-parse", "HEAD~1"])?.trim().to_string();
    Ok(Fixture {
        todos: vec![
            ("docs/plan.md:3".into(), boris.clone()),
            ("src/net.rs:2".into(), ada.clone()),
            ("src/parser.rs:4".into(), ada.clone()),
        ],
        head_author: carol.clone(),
        codename,
        review_hash,
        review_author: ada,
        review: vec![("src/lib.rs".into(), carol), ("src/net.rs".into(), boris)],
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

    /// The tracker holds exactly one review task per changed file of the
    /// fixture's review commit, each on that file's other author.
    fn review_matches(&self, fx: &Fixture) -> Res<bool> {
        let issues = self.tracker.issues("open")?;
        let mut got: Vec<(String, String)> = Vec::new();
        let mut right_commit = true;
        for i in &issues {
            match review_source(&i.source) {
                Some((file, hash)) => {
                    right_commit &= fx.review_hash.starts_with(hash);
                    got.push((file.trim_start_matches("./").to_string(), i.assignee.clone()));
                }
                None => right_commit = false,
            }
        }
        got.sort();
        let ok = right_commit && got == fx.review;
        println!(
            "[{}] в трекере {} задач(и) ревью коммита {} (автор {}), ревьюеры из истории файлов: {}",
            mark(ok),
            issues.len(),
            short_hash(&fx.review_hash),
            fx.review_author,
            issues.iter().map(|i| format!("{}→{}", i.source, i.assignee)).collect::<Vec<_>>().join(", ")
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

/// The report on disk is the last `issue_list` byte for byte, carries
/// `token` (something only the repository knows), and the last message
/// names the file.
fn file_and_notice(steps: &[ToolStep], rig: &Rig, filename: &str, token: &str) -> Res<bool> {
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
    let carried = on_disk.as_deref().is_some_and(|t| t.contains(token));
    println!("[{}] {token} из репозитория доехал до отчёта", mark(carried));
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

fn clone_step(s: &ToolStep) -> ToolStep {
    ToolStep {
        name: s.name.clone(),
        server: s.server.clone(),
        args: s.args.clone(),
        result: s.result.clone(),
        is_error: s.is_error,
        structured: s.structured.clone(),
    }
}

/// A copy of `steps` with `tamper` applied must be caught by the audit.
fn control(n: usize, what: &str, steps: &[ToolStep], tamper: impl FnOnce(&mut Vec<ToolStep>)) -> bool {
    let mut t: Vec<ToolStep> = steps.iter().map(clone_step).collect();
    tamper(&mut t);
    let caught = check_flow(&t).iter().any(|c| !c.ok);
    println!("[{}] контроль {n}: {what} — пойман аудитом", mark(caught));
    caught
}

/// The model gets `question` (no tool or server named) and the chat's
/// system note; returns its calls after printing them as lanes and audit.
fn live_flow(rig: Rig, question: &str, settings: &Settings, flow: &Flow) -> Res<(Rig, Vec<ToolStep>, bool)> {
    rig.clear_logs()?;
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
    let (_, full) = coverage(&steps, flow);
    let audited = full && checks.iter().all(|c| c.ok);
    println!("[{}] аудит флоу «{}»: все шаги ✓, покрыты все {}", mark(audited), flow.name, flow.tools.len());
    let extra: Vec<&str> = steps
        .iter()
        .filter(|s| !flow.tools.contains(&s.name.as_str()))
        .map(|s| s.name.as_str())
        .collect();
    println!(
        "    выбор инструментов: {} вызовов, вне флоу: {}",
        steps.len(),
        if extra.is_empty() { "нет".into() } else { extra.join(", ") }
    );
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
    Ok((rig, steps, audited))
}

/// `ask --verify-orchestra offline|live|all`.
///
/// A fixture repository whose authors have random names, and four real
/// servers on four ports. Two flows, each on a fresh tracker and outbox:
///
/// * triage — three `TODO:` markers; the assignee of each is the author of
///   *its file*, not of HEAD;
/// * review — the latest commit touching `src/` (not HEAD) changed two
///   files; the reviewer of each is the latest *other* author of that file,
///   a different person per file and never the commit's author.
///
/// offline — [`run_triage`] / [`run_review`] through the toolbox: Confirmed
/// only if the audit is all ✓ with every step of the flow, every server's
/// *own* call log equals what was routed to it, the tracker holds exactly
/// the expected tasks on the expected people, the report on disk equals
/// `issue_list`, and the outbox message names it. Seven controls must
/// fail: steps moved before their producers, assignees from the wrong
/// history, a call tagged with the wrong server, a source pointing at
/// another commit, and a real name collision that has to be routed by
/// qualified name to the right one of two servers.
///
/// live — the *model* gets one sentence per flow that names no tool and no
/// server; the same checks.
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
    let short = short_hash(&fx.review_hash).to_string();
    println!(
        "репозиторий-фикстура: {} — метки TODO: {}; HEAD от {}",
        repo.display(),
        fx.todos.iter().map(|(s, a)| format!("{s} ({a})")).collect::<Vec<_>>().join(", "),
        fx.head_author
    );
    println!(
        "  коммит для ревью {short} (автор {}, последний в src/, не HEAD): {}",
        fx.review_author,
        fx.review.iter().map(|(f, r)| format!("{f} → ревьюер {r}")).collect::<Vec<_>>().join(", ")
    );
    let mut all_ok = true;

    if offline {
        println!("\n== offline 1/2: триаж через 4 сервера ==");
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
        let c1 = control(1, "issue_create перед search", &rep.steps, |t| {
            let k = t.iter().position(|s| s.name == "issue_create").unwrap_or(0);
            let moved = t.remove(k);
            t.insert(0, moved);
        });
        let head = fx.head_author.clone();
        let c2 = control(2, &format!("исполнитель = автор HEAD ({head}), а не файла"), &rep.steps, |t| {
            if let Some(s) = t.iter_mut().find(|s| s.name == "issue_create") {
                s.args["assignee"] = json!(head);
            }
        });
        let c3 = control(3, "issue_create, записанный на notify", &rep.steps, |t| {
            if let Some(s) = t.iter_mut().find(|s| s.name == "issue_create") {
                s.server = NOTIFY.into();
            }
        });
        let c4 = collision_control(&repo, &base.0)?;
        let triage_ok = rep.ok() && routed && tracked && delivered && c1 && c2 && c3 && c4;

        println!("\n== offline 2/2: ревью последнего коммита в src/ через 4 сервера ==");
        let rig = Rig::new(&repo, &base.0, "offline-review")?;
        let mut tb = rig.tb;
        let file = format!("review-{code}.md");
        let req = ReviewRequest::new(None, Some("src"), Some(&file));
        let rep = run_review(&mut tb, &req, &lanes, &mut |l| println!("{l}"))?;
        let rig = Rig { tb, ..rig };
        println!("[{}] аудит флоу «ревью»: все шаги ✓, покрыты все 6", mark(rep.ok()));
        let routed = rig.routing_matches(&rep.steps)?;
        let tracked = rig.review_matches(&fx)?;
        let delivered = file_and_notice(&rep.steps, &rig, &file, &short)?;
        let author = fx.review_author.clone();
        let c5 = control(5, &format!("ревьюер = автор коммита ({author}) — сам себе ревью"), &rep.steps, |t| {
            if let Some(s) = t.iter_mut().find(|s| s.name == "issue_create") {
                s.args["assignee"] = json!(author);
            }
        });
        let c6 = control(6, "issue_create перед git_show", &rep.steps, |t| {
            let k = t.iter().position(|s| s.name == "issue_create").unwrap_or(0);
            let moved = t.remove(k);
            t.insert(0, moved);
        });
        let c7 = control(7, "source указывает на другой коммит", &rep.steps, |t| {
            if let Some(s) = t.iter_mut().find(|s| s.name == "issue_create") {
                let src = s.args["source"].as_str().unwrap_or("").to_string();
                let file = src.rsplit_once('@').map(|(f, _)| f).unwrap_or(&src).to_string();
                s.args["source"] = json!(format!("{file}@0000000"));
            }
        });
        let review_ok = rep.ok() && routed && tracked && delivered && c5 && c6 && c7;
        let ok = triage_ok && review_ok;
        println!(
            "offline: {} (триаж {}, ревью {})",
            if ok { "Confirmed" } else { "Flat" },
            mark(triage_ok),
            mark(review_ok)
        );
        all_ok &= ok;
    }

    if live {
        println!("\n== live 1/2: триаж собирает модель {} ==", settings.model);
        let file = format!("triage-live-{code}.md");
        let question = format!(
            "Разбери TODO-метки в проекте: на каждую заведи задачу в трекере на того, кто последним менял \
             этот файл. Потом сохрани список открытых задач в файл {file} и сообщи команде в канал team, \
             где лежит отчёт."
        );
        let (rig, steps, audited) = live_flow(Rig::new(&repo, &base.0, "live")?, &question, settings, &TRIAGE)?;
        let routed = rig.routing_matches(&steps)?;
        let tracked = rig.tracker_matches(&fx)?;
        let delivered = file_and_notice(&steps, &rig, &file, &fx.codename)?;
        let triage_ok = audited && routed && tracked && delivered;

        println!("\n== live 2/2: ревью собирает модель {} ==", settings.model);
        let file = format!("review-live-{code}.md");
        let question = format!(
            "Сделай ревью последнего коммита, который менял папку src: на каждый изменённый в нём файл \
             заведи в трекере задачу на ревью. Ревьюер файла — последний, кто менял этот файл, кроме \
             автора коммита (если других не было — сам автор). Сохрани список задач этого ревью в файл \
             {file} и сообщи в канал dev, где лежит чек-лист."
        );
        let (rig, steps, audited) = live_flow(Rig::new(&repo, &base.0, "live-review")?, &question, settings, &REVIEW)?;
        let routed = rig.routing_matches(&steps)?;
        let tracked = rig.review_matches(&fx)?;
        let delivered = file_and_notice(&steps, &rig, &file, &short)?;
        let review_ok = audited && routed && tracked && delivered;
        let ok = triage_ok && review_ok;
        println!(
            "live: {} (триаж {}, ревью {})",
            if ok { "Confirmed" } else { "Flat" },
            mark(triage_ok),
            mark(review_ok)
        );
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
        // A look at the tracker before the flow is not an order error, unless that
        // early (incomplete) list is what gets saved.
        let early = |s: &mut Vec<ToolStep>, digest: &str| {
            let l = &s[7];
            let mut structured = l.structured.clone();
            structured["digest"] = json!(digest);
            let step = ToolStep {
                name: l.name.clone(),
                server: l.server.clone(),
                args: json!({}),
                result: "0 задач".into(),
                is_error: false,
                structured,
            };
            s.insert(0, step);
        };
        assert_eq!(bad(&|s| early(s, "0000early")), 0, "issue_list as a first look");
        assert!(
            bad(&|s| {
                early(s, "0000early");
                s[9].structured["input"]["digest"] = json!("0000early");
            }) > 0,
            "early list saved to the file"
        );
        // Closing a task from an earlier chat turn: only the id the user named passes.
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
        steps.push(ToolStep {
            name: "issue_close".into(),
            server: TRACKER.into(),
            args: json!({"id": "T-99"}),
            result: "T-99 закрыта".into(),
            is_error: false,
            structured: json!({"id": "T-99"}),
        });
        let failed = |req: &str| check_turn(&steps, req).iter().filter(|c| !c.ok).count();
        assert_eq!(failed("закрой T-99, её завели вчера"), 0);
        assert!(failed("закрой T-9") > 0, "an id the user did not name");
        assert!(mentions_id("закрыл T-6, осталось 9", "T-6") && !mentions_id("T-60", "T-6"));
        // saveToFile fed with an edited table: its input digest no longer
        // matches any earlier output.
        let edited = tb
            .call_tool("saveToFile", json!({"filename": "b.md", "content": format!("{}\nP.S.", rep.table)}))
            .unwrap();
        assert!(bad(&|s| s[8].structured = edited.structured.clone()) > 0, "edited hand-off");
    }

    #[test]
    fn review_walks_the_commit_and_assigns_other_authors() {
        let base = TempDir(std::env::temp_dir().join(format!("ask-orchestra-review-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&base.0);
        let repo = base.0.join("repo");
        let fx = fixture_repo(&repo, "R").unwrap();
        let rig = Rig::new(&repo, &base.0, "r").unwrap();
        let mut tb = rig.tb;
        let lanes = tb.lanes();
        let mut seen = Vec::new();
        let req = ReviewRequest::new(None, Some("src"), Some("r.md"));
        let rep = run_review(&mut tb, &req, &lanes, &mut |l| seen.push(l)).unwrap();
        let rig = Rig { tb, ..rig };
        assert!(rep.ok(), "{seen:#?}");
        let names: Vec<&str> = rep.steps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            ["git_log", "git_show", "git_log", "git_log", "issue_create", "issue_create", "issue_list",
             "saveToFile", "notify_send"]
        );
        assert_eq!(server_path(&rep.steps), "git → tracker → pipeline → notify");
        // Not HEAD (Carol's README) but the latest commit in src/, by Ada.
        assert!(fx.review_hash.starts_with(rep.steps[1].args["rev"].as_str().unwrap()));
        assert!(rig.routing_matches(&rep.steps).unwrap());
        assert!(rig.review_matches(&fx).unwrap());
        assert!(file_and_notice(&rep.steps, &rig, "r.md", short_hash(&fx.review_hash)).unwrap());
        assert!(seen.iter().any(|l| l.contains("флоу «ревью»") && l.contains("все 6 шагов")), "{seen:#?}");
        // Explicit rev, second run: same commit, no duplicates.
        let mut tb = rig.tb;
        let again = run_review(&mut tb, &ReviewRequest::new(Some(&fx.review_hash), None, None), &[], &mut |_| {}).unwrap();
        assert!(again.ok());
        assert_eq!(again.steps[0].name, "git_show");
        assert_eq!(rig.tracker.issues("all").unwrap().len(), 2);
        // Self-review and a source from another commit are caught.
        let tamper = |f: &dyn Fn(&mut Vec<ToolStep>)| {
            let mut t: Vec<ToolStep> = rep.steps.iter().map(clone_step).collect();
            f(&mut t);
            check_flow(&t).iter().any(|c| !c.ok)
        };
        assert!(!tamper(&|_| {}));
        assert!(tamper(&|t| t[4].args["assignee"] = json!(fx.review_author)));
        assert!(tamper(&|t| t[4].args["source"] = json!("src/lib.rs@0000000")));
        assert!(tamper(&|t| t[1].args["rev"] = json!("0000000")));
        assert!(tamper(&|t| t[2].args["path"] = json!("README.md")));
        // Without git_show the file's own git_log still ties it to the commit.
        assert!(!tamper(&|t| {
            t.remove(1);
        }));
    }

    /// The real-checkout case: a folder on disk that was never committed on
    /// the current branch. Review stops and says why; triage files tasks
    /// without an assignee and the audit accepts exactly that.
    #[test]
    fn flows_explain_a_folder_that_was_never_committed() {
        let base = TempDir(std::env::temp_dir().join(format!("ask-orchestra-wip-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&base.0);
        let repo = base.0.join("repo");
        fixture_repo(&repo, "W").unwrap();
        std::fs::create_dir_all(repo.join("wip")).unwrap();
        std::fs::write(repo.join("wip/new.rs"), "pub fn f() {}\n// TODO: новая фича\n").unwrap();
        let rig = Rig::new(&repo, &base.0, "w").unwrap();
        let mut tb = rig.tb;
        let lanes = tb.lanes();

        let mut seen = Vec::new();
        let err = run_review(&mut tb, &ReviewRequest::new(None, Some("wip"), None), &lanes, &mut |l| seen.push(l))
            .err()
            .unwrap();
        assert!(err.contains("не закоммичен на ветке main"), "{err}");
        assert!(seen.iter().any(|l| l.starts_with("итог: wip не закоммичен") && l.contains("git add wip")), "{seen:#?}");
        assert!(seen.iter().any(|l| l.contains("wip → не закоммичен на ветке main")), "{seen:#?}");
        let missing = run_review(&mut tb, &ReviewRequest::new(None, Some("nope"), None), &[], &mut |_| {}).err().unwrap();
        assert!(missing.contains("no commits match: nope does not exist"), "{missing}");

        let mut seen = Vec::new();
        let mut req = TriageRequest::new(None, Some("w.md"));
        req.path = Some("wip".into());
        let rep = run_triage(&mut tb, &req, &lanes, &mut |l| seen.push(l)).unwrap();
        assert!(rep.ok(), "{seen:#?}");
        assert_eq!(rep.steps[1].structured["path_state"]["state"], "untracked");
        assert!(seen.iter().any(|l| l.starts_with("внимание: у 1 из 1 файлов нет истории")), "{seen:#?}");
        assert!(seen.iter().any(|l| l.contains("без исполнителя: файл не закоммичен")), "{seen:#?}");
        // An assignee invented for a file with no history is still caught.
        let mut t: Vec<ToolStep> = rep.steps.iter().map(clone_step).collect();
        t[2].args["assignee"] = json!("AdaW");
        assert!(check_flow(&t).iter().any(|c| !c.ok));
    }

    #[test]
    fn close_and_notify_are_tied_to_the_listed_task() {
        let step = |name: &str, server: &str, args: Value, structured: Value| ToolStep {
            name: name.into(),
            server: server.into(),
            args,
            result: String::new(),
            is_error: false,
            structured,
        };
        let flow = |close: &str, text: &str| {
            vec![
                step("issue_list", TRACKER, json!({}), json!({"issues": [{"id": "T-6"}, {"id": "T-7"}]})),
                step("issue_close", TRACKER, json!({"id": close}), json!({"id": close, "closed": true})),
                step("notify_send", NOTIFY, json!({"channel": "dev", "text": text}), json!({"id": "m1"})),
            ]
        };
        let bad = |steps: Vec<ToolStep>| check_flow(&steps).iter().filter(|c| !c.ok).count();
        assert_eq!(bad(flow("T-6", "закрыл T-6, осталось 1")), 0);
        assert_eq!(bad(flow("T-6", "готово")), 1, "message names no task");
        assert_eq!(bad(flow("T-9", "закрыл T-9")), 1, "closed a task nobody listed");
    }

    #[test]
    fn flow_args_split_into_main_scope_and_file() {
        let s = |r: &str| split_flow_args(r);
        assert_eq!(s(""), (String::new(), None, None));
        assert_eq!(s("panic! in tree/task-20/src > p.md"), ("panic!".into(), Some("tree/task-20/src".into()), Some("p.md".into())));
        assert_eq!(s("in src"), (String::new(), Some("src".into()), None));
        assert_eq!(s("68b003e"), ("68b003e".into(), None, None));
        assert_eq!(s("HEAD~1 > r.md"), ("HEAD~1".into(), None, Some("r.md".into())));
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
        assert_eq!(todo_title("panic!(\"no result\");", "panic!"), "panic!(\"no result\");");
        for real in ["// TODO: x", "# TODO: y", "- TODO: z", "/// TODO: doc", "<!-- TODO: html -->", " * TODO: block"] {
            assert!(marker_in_comment(real, "TODO:"), "{real}");
        }
        for mention in ["//! pipeline.search «TODO:» → …", "next: &[free(\"TODO:\")],", "find `TODO:` markers", "x // see TODO: later"] {
            assert!(!marker_in_comment(mention, "TODO:"), "{mention}");
        }
        assert_eq!(alias("ask-pipeline-mcp"), "pipeline");
        assert_eq!(alias("DeepWiki"), "DeepWiki");
    }
}
