//! Own MCP server (task 17): a tool wrapper around one local git repository,
//! spoken over the same Streamable HTTP transport the client in `mcp.rs`
//! uses — JSON-RPC 2.0 in POST bodies, plain JSON replies.
//!
//! Tools (each registered with a JSON Schema for its input):
//! * `git_log`    — recent commits, optional `limit` / `author` / `path`;
//! * `git_show`   — one commit: metadata, full message, changed files;
//! * `git_status` — current branch and uncommitted changes.
//!
//! Built on `std::net` only: one request per connection (`Connection:
//! close`), bound to 127.0.0.1. The JSON-RPC dispatch ([`Server::handle`]) is
//! a pure function of the message, so tests exercise it without a socket.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::Res;

pub const DEFAULT_PORT: u16 = 8765;
const PROTOCOL_VERSION: &str = "2025-06-18";
const MAX_BODY: usize = 1 << 20;

/// Every `tools/call` the server has executed, as `name {args}`. The proof
/// in `mcp_agent` reads it to tell "the model called the tool" apart from
/// "the model guessed".
pub type CallLog = Arc<Mutex<Vec<String>>>;

/// A server's tool implementation: `(name, arguments)` → `(text, structuredContent)`.
pub type ToolFn<'a> = &'a dyn Fn(&str, &Value) -> Res<(String, Value)>;

#[derive(Clone)]
pub struct Server {
    repo: PathBuf,
    pub calls: CallLog,
    /// Echo each request to stderr (the foreground `--mcp-serve` mode).
    verbose: bool,
}

impl Server {
    pub fn new(repo: &Path, verbose: bool) -> Res<Server> {
        let repo = repo
            .canonicalize()
            .map_err(|e| format!("repo {}: {e}", repo.display()))?;
        git(&repo, &["rev-parse", "--git-dir"])
            .map_err(|e| format!("{} is not a git repository: {e}", repo.display()))?;
        Ok(Server {
            repo,
            calls: Arc::default(),
            verbose,
        })
    }

    pub fn repo(&self) -> &Path {
        &self.repo
    }

    /// Accept connections forever (foreground mode, or a background thread).
    pub fn serve(&self, listener: TcpListener) {
        for stream in listener.incoming().flatten() {
            if let Err(e) = self.handle_connection(stream) {
                if self.verbose {
                    eprintln!("[mcp-git] connection error: {e}");
                }
            }
        }
    }

    /// Bind 127.0.0.1:`port` (0 = any free port) and serve on a background
    /// thread. Returns the endpoint URL.
    pub fn spawn(&self, port: u16) -> Res<String> {
        let listener = TcpListener::bind(("127.0.0.1", port))
            .map_err(|e| format!("bind 127.0.0.1:{port}: {e}"))?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        let server = self.clone();
        std::thread::spawn(move || server.serve(listener));
        Ok(format!("http://{addr}/mcp"))
    }

    fn handle_connection(&self, stream: TcpStream) -> Res<()> {
        handle_connection(stream, "mcp-git", self.verbose, &|msg| self.handle(msg))
    }

    /// JSON-RPC dispatch. `None` for notifications.
    pub fn handle(&self, msg: &Value) -> Option<Value> {
        let info = ServerInfo {
            name: "ask-git-mcp",
            instructions: format!("Read-only access to the git repository at {}.", self.repo.display()),
        };
        dispatch(msg, &info, &tool_specs(), &self.calls, &|name, args| self.call(name, args))
    }

    fn call(&self, name: &str, args: &Value) -> Res<(String, Value)> {
        match name {
            "git_log" => self.git_log(args),
            "git_show" => self.git_show(args),
            "git_status" => self.git_status(),
            _ => Err(format!("unknown tool: {name}")),
        }
    }

    fn git_log(&self, args: &Value) -> Res<(String, Value)> {
        let limit = match &args["limit"] {
            Value::Null => 10,
            v => v
                .as_u64()
                .filter(|n| (1..=50).contains(n))
                .ok_or("`limit` must be an integer from 1 to 50")?,
        };
        let mut cmd = vec![
            "log".to_string(),
            format!("-n{limit}"),
            "--format=%H%x1f%an%x1f%ae%x1f%aI%x1f%s".to_string(),
        ];
        if let Some(a) = opt_str(args, "author")? {
            cmd.push(format!("--author={a}"));
        }
        if let Some(p) = opt_str(args, "path")? {
            cmd.push("--".into());
            cmd.push(no_dash(p, "path")?.to_string());
        }
        let refs: Vec<&str> = cmd.iter().map(String::as_str).collect();
        let out = git(&self.repo, &refs)?;
        let commits: Vec<Value> = out
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split('\u{1f}').collect();
                (f.len() == 5).then(|| {
                    json!({"hash": f[0], "author": f[1], "email": f[2], "date": f[3], "subject": f[4]})
                })
            })
            .collect();
        let text = if commits.is_empty() {
            "no commits match".to_string()
        } else {
            commits
                .iter()
                .map(|c| {
                    let hash = c["hash"].as_str().unwrap_or("");
                    format!(
                        "{} {} {}: {}",
                        &hash[..hash.len().min(7)],
                        c["date"].as_str().unwrap_or(""),
                        c["author"].as_str().unwrap_or(""),
                        c["subject"].as_str().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        Ok((text, json!({"commits": commits})))
    }

    fn git_show(&self, args: &Value) -> Res<(String, Value)> {
        let rev = args["rev"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or("`rev` (string) is required")?;
        let rev = no_dash(rev, "rev")?;
        let out = git(
            &self.repo,
            &["show", "--stat", "--format=commit %H%nAuthor: %an <%ae>%nDate:   %aI%n%n%B", rev, "--"],
        )?;
        Ok((out.trim_end().to_string(), json!({"rev": rev, "show": out.trim_end()})))
    }

    fn git_status(&self) -> Res<(String, Value)> {
        let out = git(&self.repo, &["status", "--porcelain=v1", "--branch"])?;
        let mut lines = out.lines();
        let branch = lines
            .next()
            .and_then(|l| l.strip_prefix("## "))
            .unwrap_or("?")
            .to_string();
        let changes: Vec<&str> = lines.collect();
        let text = if changes.is_empty() {
            format!("branch: {branch}\nworking tree clean")
        } else {
            format!("branch: {branch}\n{}", changes.join("\n"))
        };
        Ok((text, json!({"branch": branch, "changes": changes})))
    }
}

/// What `tools/list` returns: name, description and JSON Schema of the input.
pub fn tool_specs() -> Vec<Value> {
    vec![
        json!({
            "name": "git_log",
            "description": "List recent commits of the repository, newest first: short hash, author date, author and subject.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "minimum": 1, "maximum": 50, "description": "How many commits to return (default 10)."},
                    "author": {"type": "string", "description": "Only commits whose author name or email contains this text."},
                    "path": {"type": "string", "description": "Only commits that touched this file or directory."},
                },
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "git_show",
            "description": "Show one commit: full hash, author, date, the whole commit message and the list of changed files.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "rev": {"type": "string", "description": "Commit hash, branch, tag or expression like HEAD~2."},
                },
                "required": ["rev"],
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "git_status",
            "description": "Current branch (with ahead/behind info) and uncommitted changes in the working tree.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
        }),
    ]
}

fn opt_str<'a>(args: &'a Value, key: &str) -> Res<Option<&'a str>> {
    match &args[key] {
        Value::Null => Ok(None),
        Value::String(s) if s.trim().is_empty() => Ok(None),
        Value::String(s) => Ok(Some(s.as_str())),
        _ => Err(format!("`{key}` must be a string")),
    }
}

/// Arguments land in git's argv; a leading `-` would turn them into options.
fn no_dash<'a>(s: &'a str, key: &str) -> Res<&'a str> {
    if s.starts_with('-') {
        Err(format!("`{key}` must not start with '-'"))
    } else {
        Ok(s)
    }
}

pub fn git(repo: &Path, args: &[&str]) -> Res<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Who answers `initialize`. Shared by every own MCP server in this binary
/// (the git one here, the scheduler in `scheduler.rs`).
pub struct ServerInfo {
    pub name: &'static str,
    pub instructions: String,
}

/// JSON-RPC dispatch common to the own MCP servers: `initialize`, `ping`,
/// `tools/list` from `specs`, `tools/call` through `call` (logged in
/// `calls`). A tool failure is an `isError` result, not a protocol error.
/// `None` for notifications.
pub fn dispatch(
    msg: &Value,
    info: &ServerInfo,
    specs: &[Value],
    calls: &CallLog,
    call: ToolFn,
) -> Option<Value> {
    let id = msg.get("id").cloned()?;
    let method = msg["method"].as_str().unwrap_or("");
    let params = &msg["params"];
    Some(match method {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": info.name, "version": env!("CARGO_PKG_VERSION")},
                "instructions": info.instructions,
            },
        }),
        "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
        "tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": specs}}),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("");
            let args = match &params["arguments"] {
                Value::Null => json!({}),
                v => v.clone(),
            };
            if !specs.iter().any(|t| t["name"] == name) {
                return Some(rpc_error(id, -32602, &format!("unknown tool: {name}")));
            }
            if let Ok(mut log) = calls.lock() {
                log.push(format!("{name} {args}"));
            }
            let result = match call(name, &args) {
                Ok((text, structured)) => json!({
                    "content": [{"type": "text", "text": text}],
                    "structuredContent": structured,
                    "isError": false,
                }),
                Err(e) => json!({
                    "content": [{"type": "text", "text": e}],
                    "isError": true,
                }),
            };
            json!({"jsonrpc": "2.0", "id": id, "result": result})
        }
        other => rpc_error(id, -32601, &format!("method not found: {other}")),
    })
}

/// One HTTP request of the Streamable HTTP transport: parse the POST, run
/// `handle` on the JSON-RPC message, write the reply. `tag` prefixes the
/// verbose request log.
pub fn handle_connection(
    mut stream: TcpStream,
    tag: &str,
    verbose: bool,
    handle: &dyn Fn(&Value) -> Option<Value>,
) -> Res<()> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| e.to_string())?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();

    let mut content_length = 0usize;
    let mut origin: Option<String> = None;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).map_err(|e| e.to_string())? == 0 {
            break;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => content_length = v.trim().parse().unwrap_or(0),
                "origin" => origin = Some(v.trim().to_string()),
                _ => {}
            }
        }
    }

    // DNS-rebinding guard from the MCP transport spec: a browser page
    // from another origin must not reach a localhost server.
    if let Some(o) = &origin {
        let local = ["http://127.0.0.1", "http://localhost"]
            .iter()
            .any(|p| o.starts_with(p));
        if !local {
            return respond(&mut stream, "403 Forbidden", None, "");
        }
    }
    if path != "/mcp" && path != "/" {
        return respond(&mut stream, "404 Not Found", None, "");
    }
    match method.as_str() {
        "POST" => {}
        // No server-initiated SSE stream; session teardown is a no-op.
        "DELETE" => return respond(&mut stream, "200 OK", None, ""),
        _ => return respond(&mut stream, "405 Method Not Allowed", None, ""),
    }
    if content_length > MAX_BODY {
        return respond(&mut stream, "413 Payload Too Large", None, "");
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).map_err(|e| e.to_string())?;

    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            let err = rpc_error(Value::Null, -32700, &format!("parse error: {e}"));
            return respond(&mut stream, "400 Bad Request", Some(&err), "");
        }
    };
    if verbose {
        let m = msg["method"].as_str().unwrap_or("?");
        match m {
            "tools/call" => eprintln!(
                "[{tag}] tools/call {} {}",
                msg["params"]["name"].as_str().unwrap_or("?"),
                msg["params"]["arguments"]
            ),
            _ => eprintln!("[{tag}] {m}"),
        }
    }
    let session = (msg["method"] == "initialize").then(|| format!("{tag}-{}", std::process::id()));
    match handle(&msg) {
        Some(reply) => respond(&mut stream, "200 OK", Some(&reply), session.as_deref().unwrap_or("")),
        // Notifications get no JSON-RPC reply.
        None => respond(&mut stream, "202 Accepted", None, ""),
    }
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn respond(stream: &mut TcpStream, status: &str, body: Option<&Value>, session: &str) -> Res<()> {
    let body = body.map(Value::to_string).unwrap_or_default();
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if !body.is_empty() {
        head.push_str("Content-Type: application/json\r\n");
    }
    if status.starts_with("405") {
        head.push_str("Allow: POST, DELETE\r\n");
    }
    if !session.is_empty() {
        head.push_str(&format!("Mcp-Session-Id: {session}\r\n"));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .and_then(|_| stream.write_all(body.as_bytes()))
        .map_err(|e| e.to_string())
}

/// Throwaway repository with known content, for tests and `--verify-mcp`.
/// The subject of HEAD is `subject`; there are two earlier commits.
pub fn fixture_repo(dir: &Path, subject: &str, author: &str) -> Res<()> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    git(dir, &["init", "-q", "-b", "main"])?;
    let commits = [
        ("README.md", "initial commit", "Alice <alice@example.com>"),
        ("src.txt", "add sources", "Bob <bob@example.com>"),
        ("proof.txt", subject, author),
    ];
    for (file, msg, who) in commits {
        std::fs::write(dir.join(file), msg).map_err(|e| e.to_string())?;
        git(dir, &["add", file])?;
        git(
            dir,
            &[
                "-c", "user.name=fixture", "-c", "user.email=fixture@example.com",
                "-c", "commit.gpgsign=false",
                "commit", "-q", "--author", who, "-m", msg,
            ],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::Connection;

    fn temp_repo(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ask-mcp-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        fixture_repo(&dir, "fix: the answer is 42", "Carol <carol@example.com>").unwrap();
        dir
    }

    #[test]
    fn dispatch_registers_three_tools_with_schemas() {
        let dir = temp_repo("list");
        let s = Server::new(&dir, false).unwrap();
        let r = s.handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})).unwrap();
        let tools = r["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["git_log", "git_show", "git_status"]);
        assert!(tools.iter().all(|t| t["inputSchema"]["type"] == "object"));
        assert_eq!(tools[1]["inputSchema"]["required"], json!(["rev"]));
        // notifications get no reply
        assert!(s.handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tool_errors_are_results_not_protocol_errors() {
        let dir = temp_repo("err");
        let s = Server::new(&dir, false).unwrap();
        let call = |name: &str, args: Value| {
            s.handle(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                             "params": {"name": name, "arguments": args}}))
                .unwrap()
        };
        let r = call("git_log", json!({"limit": 0}));
        assert_eq!(r["result"]["isError"], true);
        let r = call("git_show", json!({"rev": "--output=/tmp/x"}));
        assert_eq!(r["result"]["isError"], true);
        let r = call("nope", json!({}));
        assert_eq!(r["error"]["code"], -32602);
        assert_eq!(s.calls.lock().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole wire: real socket, our client, our server.
    #[test]
    fn client_calls_server_over_http() {
        let dir = temp_repo("wire");
        let s = Server::new(&dir, false).unwrap();
        let url = s.spawn(0).unwrap();
        let mut conn = Connection::connect(&url).unwrap();
        assert_eq!(conn.server_name, "ask-git-mcp");
        assert_eq!(conn.list_tools().unwrap().len(), 3);

        let r = conn.call_tool("git_log", json!({"limit": 1})).unwrap();
        assert!(!r.is_error);
        assert!(r.text.contains("Carol: fix: the answer is 42"), "{}", r.text);
        assert_eq!(r.text.lines().count(), 1);

        let r = conn.call_tool("git_log", json!({"author": "bob"})).unwrap();
        assert!(r.text.contains("add sources") && !r.text.contains("42"), "{}", r.text);

        let r = conn.call_tool("git_show", json!({"rev": "HEAD~2"})).unwrap();
        assert!(r.text.contains("initial commit") && r.text.contains("README.md"), "{}", r.text);

        let r = conn.call_tool("git_status", json!({})).unwrap();
        assert!(r.text.contains("branch: main") && r.text.contains("clean"), "{}", r.text);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
