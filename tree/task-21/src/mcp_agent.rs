//! The agent side of task 17: MCP tools become OpenAI-style functions, the
//! model decides when to call them, and every call goes out as `tools/call`
//! to the MCP server; the result is fed back as a `role: "tool"` message
//! until the model answers in plain text.
//!
//! [`verify`] is the causal proof: a fresh repository whose last commit
//! carries a random codename the model cannot know, asked once with the
//! tools and once without.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::api::{self, Endpoint};
use crate::config::{Res, Settings};
use crate::mcp::{self, Connection};
use crate::mcp_server::{self, Server};

/// A model that keeps calling tools forever is a bug, not a long answer. The
/// orchestrated flow of task 20 (search → git_log ×N → issue_create ×N →
/// issue_list → saveToFile → notify_send) needs ~7 rounds on its own, so the
/// cap leaves room for that and a retry or two, not for an endless loop.
const MAX_ROUNDS: usize = 16;
/// Tool output fed back to the model is capped (DeepWiki pages are huge).
const MAX_TOOL_CHARS: usize = 12_000;

pub struct ToolStep {
    /// The tool's own name on its server (`search`), even when the model saw
    /// it under a qualified name (`pipeline__search`, see [`Toolbox`]).
    pub name: String,
    /// `serverInfo.name` of the server the call was routed to — the
    /// orchestration audit (task 20) checks it against the tool's owner.
    pub server: String,
    pub args: Value,
    pub result: String,
    pub is_error: bool,
    /// `structuredContent` of the result — the pipeline audit (task 19)
    /// checks the hand-offs between tools by the digests in it.
    pub structured: Value,
}

/// Whatever can execute a `tools/call`: one MCP connection, or the chat's
/// [`Toolbox`] that routes each tool to the server it came from.
pub trait ToolCaller {
    fn call_tool(&mut self, name: &str, args: Value) -> Res<mcp::CallResult>;

    /// `(server, tool)` a function name the model used is routed to.
    fn resolve(&self, name: &str) -> (String, String) {
        (String::new(), name.to_string())
    }
}

impl ToolCaller for Connection {
    fn call_tool(&mut self, name: &str, args: Value) -> Res<mcp::CallResult> {
        Connection::call_tool(self, name, args)
    }

    fn resolve(&self, name: &str) -> (String, String) {
        (self.server_name.clone(), name.to_string())
    }
}

pub struct Round {
    pub finish_reason: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

pub struct AgentRun {
    pub answer: String,
    pub steps: Vec<ToolStep>,
    pub rounds: Vec<Round>,
    pub model: String,
}

impl AgentRun {
    pub fn footer(&self) -> String {
        let rounds: Vec<String> = self
            .rounds
            .iter()
            .map(|r| format!("{} ({}→{})", r.finish_reason, r.prompt_tokens, r.completion_tokens))
            .collect();
        format!(
            "модель {}, вызовов MCP: {}, раундов: {} [{}]",
            self.model,
            self.steps.len(),
            self.rounds.len(),
            rounds.join(", ")
        )
    }
}

/// MCP `tools/list` entry → `tools[]` entry of a chat completion.
pub fn to_function(tool: &mcp::Tool) -> Value {
    named_function(tool, &tool.name)
}

/// Same, under the name the model will see (a qualified one on collision).
fn named_function(tool: &mcp::Tool, name: &str) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": tool.description,
            "parameters": tool.input_schema,
        },
    })
}

fn system_prompt(server: &str) -> String {
    format!(
        "You are an agent connected to the MCP server `{server}`. When the question \
         needs facts that its tools can provide, call the tools instead of guessing, \
         then answer from their results. Answer in the language of the question."
    )
}

/// One question through the tool loop. `on_step` sees every MCP call as it
/// happens (the CLI prints them).
pub fn run(
    ep: &Endpoint,
    settings: &Settings,
    conn: &mut Connection,
    tools: &[mcp::Tool],
    question: &str,
    on_step: &mut dyn FnMut(&ToolStep),
) -> Res<AgentRun> {
    let functions: Vec<Value> = tools.iter().map(to_function).collect();
    let system = system_prompt(&conn.server_name);
    let messages = vec![json!({"role": "user", "content": question})];
    let (last, steps, rounds) = tool_loop(ep, settings, &system, messages, conn, &functions, on_step)?;
    Ok(AgentRun {
        answer: last.text().to_string(),
        steps,
        rounds,
        model: last.model.unwrap_or_default(),
    })
}

/// The loop itself, shared by the CLI (`run`) and the chat (`Agent` with a
/// [`Toolbox`]): ask the model with `functions`, execute every `tool_calls`
/// entry as MCP `tools/call`, feed the results back as `role: "tool"`, and
/// stop at the first answer without tool calls. The returned outcome is that
/// last answer, with `usage` summed over every round.
pub fn tool_loop(
    ep: &Endpoint,
    settings: &Settings,
    system: &str,
    mut messages: Vec<Value>,
    conn: &mut dyn ToolCaller,
    functions: &[Value],
    on_step: &mut dyn FnMut(&ToolStep),
) -> Res<(api::Outcome, Vec<ToolStep>, Vec<Round>)> {
    let mut steps = Vec::new();
    let mut rounds = Vec::new();
    let mut usage = api::Usage::default();
    for _ in 0..MAX_ROUNDS {
        let mut out = api::chat_with_tools(ep, settings, system, &messages, functions)?;
        rounds.push(Round {
            finish_reason: out.finish_reason.clone().unwrap_or_else(|| "?".into()),
            prompt_tokens: out.usage.prompt_tokens,
            completion_tokens: out.usage.completion_tokens,
        });
        usage.prompt_tokens += out.usage.prompt_tokens;
        usage.completion_tokens += out.usage.completion_tokens;
        usage.reasoning_tokens += out.usage.reasoning_tokens;
        usage.total_tokens += out.usage.total_tokens;
        let message = out.raw["choices"][0]["message"].clone();
        let calls = message["tool_calls"].as_array().cloned().unwrap_or_default();
        if calls.is_empty() {
            out.usage = usage;
            return Ok((out, steps, rounds));
        }
        // The assistant turn goes back as-is (tool_calls + reasoning), so the
        // model sees its own request next to the answers.
        messages.push(message);
        for call in calls {
            let called = call["function"]["name"].as_str().unwrap_or("");
            let (server, name) = conn.resolve(called);
            let args = match &call["function"]["arguments"] {
                Value::String(s) if s.trim().is_empty() => json!({}),
                Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
                Value::Null => json!({}),
                v => v.clone(),
            };
            let (result, is_error, structured) = match conn.call_tool(called, args.clone()) {
                Ok(r) => (r.text, r.is_error, r.structured),
                Err(e) => (e, true, Value::Null),
            };
            let step = ToolStep { name, server, args, result, is_error, structured };
            on_step(&step);
            let mut content: String = step.result.chars().take(MAX_TOOL_CHARS).collect();
            if step.is_error {
                content = format!("ERROR: {content}");
            }
            messages.push(json!({
                "role": "tool",
                "tool_call_id": call["id"],
                "content": content,
            }));
            steps.push(step);
        }
    }
    Err(format!("the model was still calling tools after {MAX_ROUNDS} rounds"))
}

/// One MCP server attached to the chat: the session and its tools.
pub struct Attached {
    pub conn: Connection,
    pub tools: Vec<mcp::Tool>,
    /// What is connected, for `/mcp` and the startup line.
    pub label: String,
}

/// Where a function the model sees goes: server index and the tool's own
/// name there.
pub struct Route {
    pub exposed: String,
    pub server: usize,
    pub tool: String,
}

/// The MCP servers the chat agent carries between turns (task 17 started
/// with one; task 19 composes several; task 20 registers four — git,
/// pipeline, tracker, notify — and runs long flows across them). Every
/// function the model sees has a route to exactly one server: a tool name
/// only one server has is exposed as is, a name two servers share becomes
/// `<alias>__<tool>` on both, so a call can never land on the wrong one.
pub struct Toolbox {
    pub servers: Vec<Attached>,
    /// Every tool of every server, converted to chat-completion functions.
    pub functions: Vec<Value>,
    pub routes: Vec<Route>,
    /// Every `tools/call` since the last [`Toolbox::take_log`].
    pub log: Vec<ToolStep>,
    /// Every `tools/call` as it happens, for the chat to put into the
    /// transcript while the turn is still running (task 20). Its own lock,
    /// so the UI can drain it while the turn holds the toolbox.
    pub live: Arc<Mutex<Live>>,
}

/// What the chat shows of the calls in flight: finished calls not yet taken
/// into the transcript, and the call running right now (the spinner names it
/// instead of "думаю").
#[derive(Default)]
pub struct Live {
    pub done: Vec<CallLine>,
    pub calling: Option<String>,
}

/// One finished `tools/call` as the chat prints it: the call line, then the
/// result under it.
pub struct CallLine {
    pub call: String,
    pub result: String,
    pub is_error: bool,
}

/// `tool call · mcp ask-git-mcp · git_log · запрос: {"path":"src"}` — which
/// server got which tool with which arguments.
pub fn call_line(server: &str, tool: &str, args: &Value) -> String {
    format!(
        "tool call · mcp {} · {tool} · запрос: {}",
        if server.is_empty() { "?" } else { server },
        crate::toolchain::shown_args_within(args, 200)
    )
}

/// The result as shown under its call line: the first lines of the text,
/// long lines cut, the rest counted.
pub fn result_lines(text: &str) -> String {
    const LINES: usize = 8;
    const WIDTH: usize = 160;
    let text = text.trim_end();
    let total = text.lines().count();
    let mut out: Vec<String> = text
        .lines()
        .take(LINES)
        .map(|l| {
            let mut s: String = l.chars().take(WIDTH).collect();
            if l.chars().count() > WIDTH {
                s.push('…');
            }
            s
        })
        .collect();
    if total > LINES {
        out.push(format!("… (+{} строк)", total - LINES));
    }
    if out.is_empty() {
        out.push("(пусто)".into());
    }
    out.join("\n")
}

pub type SharedToolbox = Arc<Mutex<Toolbox>>;

impl Toolbox {
    pub fn connect(url: &str, label: String) -> Res<Toolbox> {
        let mut conn = Connection::connect(url)?;
        let tools = conn.list_tools()?;
        let mut tb = Toolbox {
            servers: vec![Attached { conn, tools, label }],
            functions: Vec::new(),
            routes: Vec::new(),
            log: Vec::new(),
            live: Arc::default(),
        };
        tb.rebuild();
        Ok(tb)
    }

    /// Task 20: the tracker server (issues in SQLite at `db`).
    pub fn local_tracker(db: &Path) -> Res<Toolbox> {
        let server = crate::orchestra::Tracker::open(db, false)?;
        let url = server.spawn(0)?;
        Toolbox::connect(&url, format!("tracker: задачи в {} ({url})", db.display()))
    }

    /// Task 20: the notify server (outbox of team messages in `dir`).
    pub fn local_notify(dir: &Path) -> Res<Toolbox> {
        let server = crate::orchestra::Notify::new(dir, false)?;
        let url = server.spawn(0)?;
        Toolbox::connect(&url, format!("notify: исходящие в {} ({url})", server.outbox().display()))
    }

    /// Recompute routes and functions after the set of servers changed.
    pub fn rebuild(&mut self) {
        let mut aliases: Vec<String> = Vec::new();
        for s in &self.servers {
            let base = crate::orchestra::alias(&s.conn.server_name).to_string();
            let taken = aliases.iter().filter(|a| a.trim_end_matches(char::is_numeric) == base).count();
            aliases.push(if taken == 0 { base } else { format!("{base}{}", taken + 1) });
        }
        let mut routes = Vec::new();
        let mut functions = Vec::new();
        for (i, s) in self.servers.iter().enumerate() {
            for t in &s.tools {
                let shared = self.tools().filter(|o| o.name == t.name).count() > 1;
                let exposed = if shared { format!("{}__{}", aliases[i], t.name) } else { t.name.clone() };
                functions.push(named_function(t, &exposed));
                routes.push(Route { exposed, server: i, tool: t.name.clone() });
            }
        }
        self.routes = routes;
        self.functions = functions;
    }

    /// Start the own git MCP server for `repo` on a free local port (in this
    /// process, on a background thread) and connect to it over HTTP — the
    /// chat talks to it exactly as it would to a remote server.
    pub fn local_git(repo: &Path) -> Res<Toolbox> {
        let server = Server::new(repo, false)?;
        let url = server.spawn(0)?;
        Toolbox::connect(&url, format!("git {} ({url})", server.repo().display()))
    }

    /// Same for the pipeline server of task 19: `search` over files under
    /// `root` (or Wikipedia), `summarize`, `saveToFile` into `out`.
    pub fn local_pipeline(root: &Path, out: &Path) -> Res<Toolbox> {
        let server = crate::toolchain::Server::new(root, out, false)?;
        let url = server.spawn(0)?;
        Toolbox::connect(
            &url,
            format!("pipeline: поиск в {}, файлы в {} ({url})", server.root().display(), server.out().display()),
        )
    }

    /// Attach the servers of `other`; a server with the same name as one
    /// already attached replaces it in place (`/mcp git other/repo`), so the
    /// lanes of the flow picture keep their order.
    pub fn merge(&mut self, other: Toolbox) {
        for srv in other.servers {
            match self.servers.iter().position(|s| s.conn.server_name == srv.conn.server_name) {
                Some(i) => self.servers[i] = srv,
                None => self.servers.push(srv),
            }
        }
        self.rebuild();
    }

    pub fn tools(&self) -> impl Iterator<Item = &mcp::Tool> {
        self.servers.iter().flat_map(|s| s.tools.iter())
    }

    /// The names the model sees (qualified where two servers share a name).
    pub fn tool_names(&self) -> String {
        self.routes.iter().map(|r| r.exposed.as_str()).collect::<Vec<_>>().join(", ")
    }

    /// Short server names in attach order — the lanes of the flow picture.
    pub fn lanes(&self) -> Vec<String> {
        self.servers
            .iter()
            .map(|s| crate::orchestra::alias(&s.conn.server_name).to_string())
            .collect()
    }

    pub fn labels(&self) -> String {
        self.servers.iter().map(|s| s.label.as_str()).collect::<Vec<_>>().join("; ")
    }

    /// The connection of the server named `server_name` (`ask-pipeline-mcp`).
    pub fn server(&mut self, server_name: &str) -> Option<&mut Connection> {
        self.servers
            .iter_mut()
            .find(|s| s.conn.server_name == server_name)
            .map(|s| &mut s.conn)
    }

    pub fn take_log(&mut self) -> Vec<ToolStep> {
        std::mem::take(&mut self.log)
    }
}

impl ToolCaller for Toolbox {
    fn call_tool(&mut self, name: &str, args: Value) -> Res<mcp::CallResult> {
        let route = self.routes.iter().find(|r| r.exposed == name).ok_or_else(|| {
            let shared: Vec<&str> = self
                .routes
                .iter()
                .filter(|r| r.tool == name)
                .map(|r| r.exposed.as_str())
                .collect();
            if shared.is_empty() {
                format!("no attached MCP server has the tool `{name}`")
            } else {
                format!("`{name}` exists on several servers — call one of: {}", shared.join(", "))
            }
        });
        let route = match route {
            Ok(r) => r,
            Err(e) => {
                // Не дошедший ни до какого сервера вызов тоже виден в чате.
                if let Ok(mut l) = self.live.lock() {
                    l.done.push(CallLine { call: call_line("", name, &args), result: result_lines(&e), is_error: true });
                }
                return Err(e);
            }
        };
        let (server, tool) = (route.server, route.tool.clone());
        let line = call_line(&self.servers[server].conn.server_name, &tool, &args);
        if let Ok(mut l) = self.live.lock() {
            l.calling = Some(line.clone());
        }
        let out = self.servers[server].conn.call_tool(&tool, args);
        if let Ok(mut l) = self.live.lock() {
            l.calling = None;
            let (result, is_error) = match &out {
                Ok(r) => (result_lines(&r.text), r.is_error),
                Err(e) => (result_lines(e), true),
            };
            l.done.push(CallLine { call: line, result, is_error });
        }
        out
    }

    fn resolve(&self, name: &str) -> (String, String) {
        match self.routes.iter().find(|r| r.exposed == name) {
            Some(r) => (self.servers[r.server].conn.server_name.clone(), r.tool.clone()),
            None => (String::new(), name.to_string()),
        }
    }
}

/// Appended to the chat's own system prompt when tools are attached, so a
/// casual "глянь что там в гите" or "найди, сверни и сохрани" is read as a
/// reason to call them. Each server speaks for itself via its MCP
/// `instructions`.
pub fn chat_note(toolbox: &Toolbox) -> String {
    let servers: Vec<String> = toolbox
        .servers
        .iter()
        .map(|s| {
            format!(
                "<mcp-server name=\"{}\" tools=\"{}\">connected to {}. {}</mcp-server>",
                s.conn.server_name,
                toolbox
                    .routes
                    .iter()
                    .filter(|r| toolbox.servers[r.server].conn.server_name == s.conn.server_name)
                    .map(|r| r.exposed.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                s.label,
                s.conn.instructions
            )
        })
        .collect();
    format!(
        "<mcp-tools>You can call the tools of these MCP servers. When the request needs what \
         they provide, call the tools instead of guessing, then answer from their results.\n{}\n</mcp-tools>",
        servers.join("\n")
    )
}

/// Short one-line preview of a tool result for the console.
pub fn preview(text: &str) -> String {
    let first = text.lines().next().unwrap_or("");
    let lines = text.lines().count();
    let mut p: String = first.chars().take(90).collect();
    if first.chars().count() > 90 {
        p.push('…');
    }
    if lines > 1 {
        p.push_str(&format!("  (+{} строк)", lines - 1));
    }
    p
}

struct TempRepo(PathBuf);

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `ask --verify-mcp`: Confirmed only if the answer carries facts that exist
/// nowhere but in the fixture repository, the server logged the model's
/// `tools/call`, and the same question without tools does *not* produce them.
pub fn verify(settings: &Settings) -> Res<bool> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let codename = format!("{:08x}", (stamp as u32) ^ std::process::id().rotate_left(16));
    let author = format!("Tester{}", &codename[..4].to_uppercase());
    let subject = format!("release: codename {codename}");
    let dir = TempRepo(std::env::temp_dir().join(format!("ask-mcp-proof-{codename}")));
    mcp_server::fixture_repo(&dir.0, &subject, &format!("{author} <proof@example.com>"))?;
    println!("репозиторий-фикстура: {} (HEAD: «{subject}», автор {author})", dir.0.display());

    let server = Server::new(&dir.0, false)?;
    let url = server.spawn(0)?;
    let mut conn = Connection::connect(&url)?;
    let tools = conn.list_tools()?;
    println!(
        "сервер: {} {} на {url}, инструменты: {}",
        conn.server_name,
        conn.server_version,
        tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", ")
    );

    // 1. Server alone, no model: the tool itself returns the fact.
    let direct = conn.call_tool("git_log", json!({"limit": 1}))?;
    let direct_ok = !direct.is_error && direct.text.contains(&codename);
    println!(
        "[{}] прямой tools/call git_log {{limit:1}} → {}",
        mark(direct_ok),
        direct.text.trim()
    );
    server.calls.lock().map_err(|e| e.to_string())?.clear();

    let ep = Endpoint::for_model(&settings.model)?;
    let question = "What is the subject of the latest commit in the repository, and who is its author? \
                    Quote the subject exactly.";

    // 2. With tools: the model must go through MCP to know the codename.
    let mut on_step = |s: &ToolStep| println!("    → {} {}  ← {}", s.name, s.args, preview(&s.result));
    let with = run(&ep, settings, &mut conn, &tools, question, &mut on_step)?;
    let logged = server.calls.lock().map_err(|e| e.to_string())?.clone();
    let with_ok = with.answer.contains(&codename) && with.answer.contains(&author);
    println!(
        "[{}] с инструментами: вызовов на сервере {}, кодовое имя в ответе: {}, автор: {}",
        mark(with_ok && !logged.is_empty()),
        logged.len(),
        with.answer.contains(&codename),
        with.answer.contains(&author)
    );
    println!("    ответ: {}", one_line(&with.answer));
    println!("    {}", with.footer());

    // 3. Control: same question, no tools. The codename is random — the
    //    model can only produce it by reading the repository.
    let control = api::chat_with_tools(
        &ep,
        settings,
        &system_prompt(&conn.server_name),
        &[json!({"role": "user", "content": question})],
        &[],
    )?;
    let control_leak = control.text().contains(&codename);
    println!(
        "[{}] контроль без инструментов: кодовое имя в ответе: {} (finish_reason {}, {}→{} токенов)",
        mark(!control_leak),
        control_leak,
        control.finish_reason.as_deref().unwrap_or("?"),
        control.usage.prompt_tokens,
        control.usage.completion_tokens
    );
    println!("    ответ: {}", one_line(control.text()));

    let confirmed = direct_ok && with_ok && !logged.is_empty() && !control_leak;
    if confirmed {
        println!("\nMCP: Confirmed — агент вызвал инструмент через MCP и ответил его результатом");
    } else if logged.is_empty() {
        println!("\nMCP: Flat — модель не вызвала ни одного инструмента");
    } else {
        println!("\nMCP: Flat — инструмент вызван, но ответ не опирается на его результат");
    }
    Ok(confirmed)
}

fn mark(ok: bool) -> &'static str {
    if ok {
        "ok"
    } else {
        "FAIL"
    }
}

fn one_line(s: &str) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = s.chars().take(300).collect();
    if s.chars().count() > 300 {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_tool_becomes_function_with_schema_verbatim() {
        let spec = &mcp_server::tool_specs()[1];
        let tool = mcp::Tool {
            name: "git_show".into(),
            description: "d".into(),
            params: vec![],
            input_schema: spec["inputSchema"].clone(),
        };
        let f = to_function(&tool);
        assert_eq!(f["type"], "function");
        assert_eq!(f["function"]["name"], "git_show");
        assert_eq!(f["function"]["parameters"]["required"], json!(["rev"]));
    }

    #[test]
    fn local_git_toolbox_serves_the_repo_over_http() {
        let dir = std::env::temp_dir().join(format!("ask-toolbox-{}", std::process::id()));
        let _guard = TempRepo(dir.clone());
        mcp_server::fixture_repo(&dir, "feat: toolbox marker", "Dana <dana@example.com>").unwrap();
        let mut tb = Toolbox::local_git(&dir).unwrap();
        assert_eq!(tb.tool_names(), "git_log, git_show, git_status");
        assert_eq!(tb.functions.len(), 3);
        let note = chat_note(&tb);
        assert!(note.contains("git_log") && note.contains(&dir.canonicalize().unwrap().display().to_string()));
        let r = ToolCaller::call_tool(&mut tb, "git_log", json!({"limit": 1})).unwrap();
        assert!(!r.is_error && r.text.contains("toolbox marker") && r.text.contains("Dana"));
        assert!(tb.take_log().is_empty());
        // Каждый вызов записан для чата: строка вызова с сервером, именем и
        // запросом, под ней результат; вызов мимо всех серверов — тоже.
        let _ = ToolCaller::call_tool(&mut tb, "no_such_tool", json!({"q": 1}));
        let live = tb.live.lock().unwrap();
        assert!(live.calling.is_none());
        assert_eq!(live.done.len(), 2);
        assert_eq!(live.done[0].call, r#"tool call · mcp ask-git-mcp · git_log · запрос: {"limit":1}"#);
        assert!(live.done[0].result.contains("toolbox marker") && !live.done[0].is_error);
        assert!(live.done[1].call.starts_with("tool call · mcp ? · no_such_tool"));
        assert!(live.done[1].is_error && live.done[1].result.contains("no attached MCP server"));
    }

    #[test]
    fn result_lines_cut_long_output() {
        let text: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        let r = result_lines(&text);
        assert_eq!(r.lines().count(), 9);
        assert!(r.ends_with("… (+12 строк)"));
        assert_eq!(result_lines(""), "(пусто)");
    }
}
