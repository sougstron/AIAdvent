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

/// A model that keeps calling tools forever is a bug, not a long answer.
const MAX_ROUNDS: usize = 6;
/// Tool output fed back to the model is capped (DeepWiki pages are huge).
const MAX_TOOL_CHARS: usize = 12_000;

pub struct ToolStep {
    pub name: String,
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
}

impl ToolCaller for Connection {
    fn call_tool(&mut self, name: &str, args: Value) -> Res<mcp::CallResult> {
        Connection::call_tool(self, name, args)
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
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
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
            let name = call["function"]["name"].as_str().unwrap_or("").to_string();
            let args = match &call["function"]["arguments"] {
                Value::String(s) if s.trim().is_empty() => json!({}),
                Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
                Value::Null => json!({}),
                v => v.clone(),
            };
            let (result, is_error, structured) = match conn.call_tool(&name, args.clone()) {
                Ok(r) => (r.text, r.is_error, r.structured),
                Err(e) => (e, true, Value::Null),
            };
            let step = ToolStep { name, args, result, is_error, structured };
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

/// The MCP servers the chat agent carries between turns (task 17 started
/// with one; task 19 composes several — git and the pipeline — so the
/// model can chain tools of different servers in one turn). Each tool call
/// is routed to the server that listed the tool.
pub struct Toolbox {
    pub servers: Vec<Attached>,
    /// Every tool of every server, converted to chat-completion functions.
    pub functions: Vec<Value>,
    /// Every `tools/call` since the last [`Toolbox::take_log`].
    pub log: Vec<ToolStep>,
}

pub type SharedToolbox = Arc<Mutex<Toolbox>>;

impl Toolbox {
    pub fn connect(url: &str, label: String) -> Res<Toolbox> {
        let mut conn = Connection::connect(url)?;
        let tools = conn.list_tools()?;
        let functions = tools.iter().map(to_function).collect();
        Ok(Toolbox { servers: vec![Attached { conn, tools, label }], functions, log: Vec::new() })
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
    /// already attached replaces it (`/mcp git other/repo`).
    pub fn merge(&mut self, other: Toolbox) {
        for srv in other.servers {
            self.servers.retain(|s| s.conn.server_name != srv.conn.server_name);
            self.servers.push(srv);
        }
        self.functions = self.tools().map(to_function).collect();
    }

    pub fn tools(&self) -> impl Iterator<Item = &mcp::Tool> {
        self.servers.iter().flat_map(|s| s.tools.iter())
    }

    pub fn tool_names(&self) -> String {
        self.tools().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", ")
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
        let srv = self
            .servers
            .iter_mut()
            .find(|s| s.tools.iter().any(|t| t.name == name))
            .ok_or_else(|| format!("no attached MCP server has the tool `{name}`"))?;
        srv.conn.call_tool(name, args)
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
                s.tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", "),
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
    }
}
