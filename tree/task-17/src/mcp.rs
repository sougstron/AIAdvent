//! Minimal MCP client (task 16): open a session with a remote MCP server
//! over Streamable HTTP and ask it for its tool list. Nothing is called yet —
//! the only goal is "the connection comes up and `tools/list` answers".
//!
//! Wire protocol (MCP 2025-06-18, JSON-RPC 2.0 over HTTP POST):
//! 1. `initialize` → server replies with its info and capabilities, and may
//!    hand out an `Mcp-Session-Id` header that every later request echoes;
//! 2. `notifications/initialized` (no id, no reply — server answers 202);
//! 3. `tools/list` (paginated via `nextCursor`).
//!
//! The server may answer either with plain JSON or with an SSE stream
//! (`text/event-stream`); both are handled by [`parse_body`].

use serde_json::{json, Value};

use crate::config::Res;

/// Public, auth-free MCP server used when no URL is given.
pub const DEFAULT_URL: &str = "https://mcp.deepwiki.com/mcp";
const PROTOCOL_VERSION: &str = "2025-06-18";

pub struct Tool {
    pub name: String,
    pub description: String,
    /// Names of the input-schema properties, required ones marked with `*`.
    pub params: Vec<String>,
}

pub struct Connection {
    url: String,
    session_id: Option<String>,
    next_id: u64,
    pub server_name: String,
    pub server_version: String,
    pub protocol_version: String,
}

impl Connection {
    /// Handshake: `initialize` + `notifications/initialized`.
    pub fn connect(url: &str) -> Res<Self> {
        let mut conn = Connection {
            url: url.to_string(),
            session_id: None,
            next_id: 1,
            server_name: String::new(),
            server_version: String::new(),
            protocol_version: String::new(),
        };
        let result = conn.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "ask", "version": env!("CARGO_PKG_VERSION")},
            }),
        )?;
        let info = &result["serverInfo"];
        conn.server_name = info["name"].as_str().unwrap_or("?").to_string();
        conn.server_version = info["version"].as_str().unwrap_or("?").to_string();
        conn.protocol_version = result["protocolVersion"].as_str().unwrap_or("?").to_string();
        conn.notify("notifications/initialized")?;
        Ok(conn)
    }

    /// `tools/list`, following `nextCursor` until the server stops paging.
    pub fn list_tools(&mut self) -> Res<Vec<Tool>> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let result = self.request("tools/list", params)?;
            let page = result["tools"]
                .as_array()
                .ok_or("tools/list: no `tools` array in the result")?;
            tools.extend(page.iter().map(parse_tool));
            cursor = result["nextCursor"].as_str().map(str::to_string);
            if cursor.is_none() {
                return Ok(tools);
            }
        }
    }

    fn post(&self, method: &str, body: &Value) -> Res<ureq::Response> {
        let mut req = ureq::post(&self.url)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream")
            .set("MCP-Protocol-Version", PROTOCOL_VERSION);
        if let Some(sid) = &self.session_id {
            req = req.set("Mcp-Session-Id", sid);
        }
        req.send_json(body.clone())
            .map_err(|e| http_error(method, &self.url, e))
    }

    fn request(&mut self, method: &str, params: Value) -> Res<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let resp = self.post(method, &body)?;
        if let Some(sid) = resp.header("Mcp-Session-Id") {
            self.session_id = Some(sid.to_string());
        }
        let ctype = resp.content_type().to_string();
        let text = resp
            .into_string()
            .map_err(|e| format!("{method}: reading response: {e}"))?;
        let msg = parse_body(&ctype, &text, id)?;
        if let Some(err) = msg.get("error") {
            return Err(format!("{method}: server error: {err}"));
        }
        msg.get("result")
            .cloned()
            .ok_or_else(|| format!("{method}: response has neither result nor error"))
    }

    fn notify(&self, method: &str) -> Res<()> {
        let body = json!({"jsonrpc": "2.0", "method": method});
        self.post(method, &body)?;
        Ok(())
    }
}

fn http_error(method: &str, url: &str, e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, r) => {
            let body = r.into_string().unwrap_or_default();
            let body: String = body.trim().chars().take(200).collect();
            format!("{method}: HTTP {code} from {url}: {body}")
        }
        other => format!("{method}: cannot reach {url}: {other}"),
    }
}

/// Picks the JSON-RPC response with our `id` out of a plain-JSON or SSE body.
fn parse_body(content_type: &str, text: &str, id: u64) -> Res<Value> {
    if !content_type.starts_with("text/event-stream") {
        return serde_json::from_str(text).map_err(|e| format!("bad JSON from server: {e}"));
    }
    // SSE: events are separated by blank lines; `data:` lines of one event
    // are joined with '\n'. Skip server notifications, keep our response.
    for event in text.replace("\r\n", "\n").split("\n\n") {
        let data: Vec<&str> = event
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(|d| d.strip_prefix(' ').unwrap_or(d))
            .collect();
        if data.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&data.join("\n")) else {
            continue;
        };
        if msg["id"].as_u64() == Some(id) {
            return Ok(msg);
        }
    }
    Err(format!("no response with id {id} in the event stream"))
}

fn parse_tool(v: &Value) -> Tool {
    let schema = &v["inputSchema"];
    let required: Vec<&str> = schema["required"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let params = schema["properties"]
        .as_object()
        .map(|props| {
            props
                .keys()
                .map(|k| {
                    if required.contains(&k.as_str()) {
                        format!("{k}*")
                    } else {
                        k.clone()
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    Tool {
        name: v["name"].as_str().unwrap_or("?").to_string(),
        description: v["description"].as_str().unwrap_or("").trim().to_string(),
        params,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_body_skips_notifications_and_picks_our_id() {
        let body = "event: message\r\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\r\n\r\n\
                    event: message\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\r\n\r\n";
        let msg = parse_body("text/event-stream", body, 7).unwrap();
        assert_eq!(msg["result"]["ok"], true);
        assert!(parse_body("text/event-stream", body, 8).is_err());
    }

    #[test]
    fn plain_json_body() {
        let msg = parse_body("application/json", r#"{"jsonrpc":"2.0","id":1,"result":{}}"#, 1).unwrap();
        assert!(msg["result"].is_object());
    }

    #[test]
    fn tool_params_mark_required() {
        let t = parse_tool(&json!({
            "name": "ask",
            "description": " q ",
            "inputSchema": {"properties": {"repo": {}, "q": {}}, "required": ["repo"]},
        }));
        assert_eq!(t.name, "ask");
        assert_eq!(t.description, "q");
        assert!(t.params.contains(&"repo*".to_string()));
        assert!(t.params.contains(&"q".to_string()));
    }
}
