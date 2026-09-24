//! The agent side of task 17: MCP tools become OpenAI-style functions, the
//! model decides when to call them, and every call goes out as `tools/call`
//! to the MCP server; the result is fed back as a `role: "tool"` message
//! until the model answers in plain text.
//!
//! [`verify`] is the causal proof: a fresh repository whose last commit
//! carries a random codename the model cannot know, asked once with the
//! tools and once without.

use serde_json::{json, Value};
use std::path::PathBuf;

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
    let mut messages = vec![json!({"role": "user", "content": question})];
    let mut run = AgentRun {
        answer: String::new(),
        steps: Vec::new(),
        rounds: Vec::new(),
        model: String::new(),
    };
    for _ in 0..MAX_ROUNDS {
        let out = api::chat_with_tools(ep, settings, &system, &messages, &functions)?;
        run.model = out.model.clone().unwrap_or_default();
        run.rounds.push(Round {
            finish_reason: out.finish_reason.clone().unwrap_or_else(|| "?".into()),
            prompt_tokens: out.usage.prompt_tokens,
            completion_tokens: out.usage.completion_tokens,
        });
        let message = out.raw["choices"][0]["message"].clone();
        let calls = message["tool_calls"].as_array().cloned().unwrap_or_default();
        if calls.is_empty() {
            run.answer = out.text().to_string();
            return Ok(run);
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
            let (result, is_error) = match conn.call_tool(&name, args.clone()) {
                Ok(r) => (r.text, r.is_error),
                Err(e) => (e, true),
            };
            let step = ToolStep { name, args, result, is_error };
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
            run.steps.push(step);
        }
    }
    Err(format!("the model was still calling tools after {MAX_ROUNDS} rounds"))
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
}
