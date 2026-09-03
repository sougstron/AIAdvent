//! Talking to the OpenAI-compatible endpoint. Multi-turn: the whole visible
//! history is resent every call, exactly like a real chat client.

use serde::Deserialize;
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::time::Instant;

use crate::config::{Res, Settings};

const DEFAULT_BASE_URL: &str = "https://yolo-auto.com/v1";
const DEFAULT_MODEL: &str = "qwen3.8-27b";

#[derive(Deserialize)]
struct PiModels {
    providers: std::collections::BTreeMap<String, PiProvider>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PiProvider {
    #[serde(default)]
    api_key: Option<String>,
}

pub struct Endpoint {
    pub base_url: String,
    pub model: String,
    api_key: String,
}

impl Endpoint {
    /// Unusable for actual requests — only for tests that need an `Endpoint`
    /// value but return before making a call.
    #[cfg(test)]
    pub fn dummy() -> Endpoint {
        Endpoint {
            base_url: "http://unused.invalid".into(),
            model: "unused".into(),
            api_key: String::new(),
        }
    }

    /// Resolution order: `$YOLO_BASE_URL` / `$YOLO_MODEL` / `$YOLO_API_KEY`,
    /// falling back to the `Yolo-Auto` provider in `~/.pi/agent/models.json`.
    pub fn resolve() -> Res<Endpoint> {
        Ok(Endpoint {
            base_url: env::var("YOLO_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.into()),
            model: env::var("YOLO_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into()),
            api_key: resolve_api_key()?,
        })
    }
}

fn resolve_api_key() -> Res<String> {
    if let Ok(key) = env::var("YOLO_API_KEY") {
        if !key.is_empty() {
            return Ok(key);
        }
    }
    let path = pi_models_path()?;
    let raw =
        fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    let models: PiModels = serde_json::from_str(&raw)
        .map_err(|e| format!("cannot parse {}: {}", path.display(), e))?;
    models
        .providers
        .get("Yolo-Auto")
        .and_then(|p| p.api_key.clone())
        .ok_or_else(|| format!("no Yolo-Auto apiKey found in {}", path.display()))
}

fn pi_models_path() -> Res<PathBuf> {
    env::var("HOME")
        .map(|h| PathBuf::from(h).join(".pi/agent/models.json"))
        .map_err(|_| "HOME not set".to_string())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

impl ChatMessage {
    pub fn user(s: impl Into<String>) -> ChatMessage {
        ChatMessage {
            role: Role::User,
            content: s.into(),
        }
    }
    pub fn assistant(s: impl Into<String>) -> ChatMessage {
        ChatMessage {
            role: Role::Assistant,
            content: s.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Part of `completion_tokens` spent thinking, not in the visible answer.
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
}

/// One generation, with everything needed to judge whether the stop
/// condition and length cap actually fired.
#[derive(Clone, Debug)]
pub struct Outcome {
    /// `None` when the budget ran out before any visible token was produced.
    pub content: Option<String>,
    pub reasoning: Option<String>,
    pub finish_reason: Option<String>,
    pub usage: Usage,
    pub raw: Value,
    pub latency_ms: u128,
}

impl Outcome {
    /// True when the token budget cut generation off mid-thought, rather
    /// than the model finishing on its own.
    pub fn truncated(&self) -> bool {
        matches!(self.finish_reason.as_deref(), Some("length"))
    }

    /// True when a literal stop sequence ended generation early.
    pub fn stopped_by_sequence(&self) -> bool {
        matches!(self.finish_reason.as_deref(), Some("stop"))
    }

    pub fn text(&self) -> &str {
        self.content.as_deref().unwrap_or("").trim()
    }
}

/// Builds the request body. Kept separate from `chat` so `--show-request`
/// and tests can inspect it without a network call.
pub fn build_body(
    model: &str,
    settings: &Settings,
    system: &str,
    history: &[ChatMessage],
    schema: Option<&Value>,
) -> Value {
    let mut messages = Vec::new();

    let budget_note = settings.budget_tokens.map(|n| format!(
        "The entire generation, including reasoning and the final answer, has a hard budget of {n} tokens. Plan accordingly and finish the final answer before that limit; never stop mid-answer."
    ));
    let chars_note = settings
        .max_chars
        .map(|n| format!("Keep your entire final answer under {n} characters."));
    let mut system = system.to_string();
    for note in [budget_note, chars_note].into_iter().flatten() {
        if system.is_empty() {
            system = note;
        } else {
            system = format!("{system}\n\n{note}");
        }
    }
    if !system.is_empty() {
        messages.push(json!({ "role": "system", "content": system }));
    }
    for m in history {
        messages.push(json!({ "role": m.role.as_str(), "content": m.content }));
    }

    let mut body = json!({
        "model": model,
        "messages": messages,
    });
    let obj = body.as_object_mut().expect("object");

    if settings.json_mode.enabled {
        obj.insert(
            "response_format".into(),
            json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "answer",
                    "strict": true,
                    "schema": schema.cloned().unwrap_or(json!({ "type": "object" })),
                }
            }),
        );
    }

    if let Some(n) = settings.budget_tokens {
        obj.insert("max_tokens".into(), json!(n));
    }
    if !settings.stop.is_empty() {
        obj.insert("stop".into(), json!(settings.stop));
    }
    if let Some(t) = settings.temperature {
        obj.insert("temperature".into(), json!(t));
    }

    if !settings.thinking() {
        // Two independent switches: the OpenAI-style one and the Qwen chat-template
        // one. Both were verified live to zero out reasoning_tokens on this provider.
        obj.insert("reasoning_effort".into(), json!("none"));
        obj.insert(
            "chat_template_kwargs".into(),
            json!({ "enable_thinking": false }),
        );
    } else {
        obj.insert("reasoning_effort".into(), json!(settings.effort.label()));
    }
    body
}

pub fn chat(
    ep: &Endpoint,
    settings: &Settings,
    system: &str,
    history: &[ChatMessage],
    schema: Option<&Value>,
) -> Res<Outcome> {
    let body = build_body(&ep.model, settings, system, history, schema);
    let url = format!("{}/chat/completions", ep.base_url);

    let started = Instant::now();
    let resp = ureq::post(&url)
        .set("Authorization", &format!("Bearer {}", ep.api_key))
        .set("Content-Type", "application/json")
        .send_json(body);
    let latency_ms = started.elapsed().as_millis();

    let resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let detail = r.into_string().unwrap_or_default();
            return Err(format!("HTTP {code} from {url}: {}", detail.trim()));
        }
        Err(e) => return Err(format!("request failed: {e}")),
    };

    let raw: Value = resp
        .into_json()
        .map_err(|e| format!("response was not JSON: {e}"))?;

    let choice = raw
        .get("choices")
        .and_then(|c| c.get(0))
        .ok_or_else(|| format!("no choices in response: {raw}"))?;
    let message = choice.get("message");

    // `content` is null whenever generation stopped before any visible token — a
    // real case here, since both the token budget and stop sequences can fire
    // while the model is still inside `reasoning_content`.
    let content = message
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(str::to_string);
    let reasoning = message
        .and_then(|m| m.get("reasoning_content"))
        .and_then(|c| c.as_str())
        .map(str::to_string);
    let finish_reason = choice
        .get("finish_reason")
        .and_then(|f| f.as_str())
        .map(str::to_string);

    let u = raw.get("usage");
    let usage = Usage {
        prompt_tokens: field(u, "prompt_tokens"),
        completion_tokens: field(u, "completion_tokens"),
        reasoning_tokens: u
            .and_then(|u| u.get("completion_tokens_details"))
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or_else(|| field(u, "reasoning_tokens")),
        total_tokens: field(u, "total_tokens"),
    };

    Ok(Outcome {
        content,
        reasoning,
        finish_reason,
        usage,
        raw,
        latency_ms,
    })
}

/// A live chat-completion stream: pull visible-text deltas with `next_chunk`
/// until it returns `Ok(None)`, then call `into_outcome` for the same
/// `Outcome` shape `chat` returns (usage, finish_reason, full text) — the
/// streaming and non-streaming paths converge there so callers downstream of
/// "the reply is done" don't need to care which path produced it.
pub struct ChatStream {
    lines: std::io::Lines<BufReader<Box<dyn Read + Send + Sync>>>,
    started: Instant,
    content: String,
    reasoning: String,
    finish_reason: Option<String>,
    usage: Usage,
}

impl ChatStream {
    /// Blocks until the next visible-content delta arrives, returning `None`
    /// once the stream ends. Usage, `reasoning_content`, and `finish_reason`
    /// are accumulated internally along the way and surface through
    /// `into_outcome` — a caller updating a live view only ever needs the
    /// text.
    pub fn next_chunk(&mut self) -> Res<Option<String>> {
        loop {
            let Some(line) = self.lines.next() else {
                return Ok(None);
            };
            let line = line.map_err(|e| format!("stream read failed: {e}"))?;
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            if data == "[DONE]" {
                return Ok(None);
            }
            let Ok(chunk) = serde_json::from_str::<Value>(data) else {
                continue;
            };

            if let Some(u) = chunk.get("usage").filter(|u| !u.is_null()) {
                self.usage = Usage {
                    prompt_tokens: field(Some(u), "prompt_tokens"),
                    completion_tokens: field(Some(u), "completion_tokens"),
                    reasoning_tokens: u
                        .get("completion_tokens_details")
                        .and_then(|d| d.get("reasoning_tokens"))
                        .and_then(Value::as_u64)
                        .unwrap_or_else(|| field(Some(u), "reasoning_tokens")),
                    total_tokens: field(Some(u), "total_tokens"),
                };
            }

            let Some(choice) = chunk.get("choices").and_then(|c| c.get(0)) else {
                continue;
            };
            if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                self.finish_reason = Some(fr.to_string());
            }
            let delta = choice.get("delta");
            if let Some(piece) = delta
                .and_then(|d| d.get("reasoning_content"))
                .and_then(|c| c.as_str())
            {
                self.reasoning.push_str(piece);
            }
            let content_delta = delta
                .and_then(|d| d.get("content"))
                .and_then(|c| c.as_str())
                .filter(|s| !s.is_empty());
            if let Some(piece) = content_delta {
                self.content.push_str(piece);
                return Ok(Some(piece.to_string()));
            }
        }
    }

    /// Everything accumulated so far, in the same shape `chat` returns —
    /// callable after `next_chunk` returns `Ok(None)`, or early (e.g. after
    /// a read error) to keep whatever text streamed in before the failure.
    pub fn into_outcome(self) -> Outcome {
        Outcome {
            content: (!self.content.is_empty()).then_some(self.content),
            reasoning: (!self.reasoning.is_empty()).then_some(self.reasoning),
            finish_reason: self.finish_reason,
            usage: self.usage,
            raw: Value::Null,
            latency_ms: self.started.elapsed().as_millis(),
        }
    }
}

/// Same request as `chat`, but with `stream: true` — returns a `ChatStream`
/// to pull deltas from as they arrive over the wire, instead of blocking for
/// the whole body. `schema` is unused by streaming replies today (JSON mode
/// stays on the non-streaming path in the TUI, since flattening structured
/// output only makes sense once it's complete) but kept for signature parity.
pub fn chat_stream(
    ep: &Endpoint,
    settings: &Settings,
    system: &str,
    history: &[ChatMessage],
    schema: Option<&Value>,
) -> Res<ChatStream> {
    let mut body = build_body(&ep.model, settings, system, history, schema);
    let obj = body.as_object_mut().expect("object");
    obj.insert("stream".into(), json!(true));
    obj.insert("stream_options".into(), json!({ "include_usage": true }));
    let url = format!("{}/chat/completions", ep.base_url);

    let started = Instant::now();
    let resp = ureq::post(&url)
        .set("Authorization", &format!("Bearer {}", ep.api_key))
        .set("Content-Type", "application/json")
        .send_json(body);

    let resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let detail = r.into_string().unwrap_or_default();
            return Err(format!("HTTP {code} from {url}: {}", detail.trim()));
        }
        Err(e) => return Err(format!("request failed: {e}")),
    };

    let lines = BufReader::new(resp.into_reader()).lines();
    Ok(ChatStream {
        lines,
        started,
        content: String::new(),
        reasoning: String::new(),
        finish_reason: None,
        usage: Usage::default(),
    })
}

#[cfg(test)]
impl ChatStream {
    /// Builds a stream over canned SSE bytes, bypassing the network — lets
    /// `next_chunk`/`into_outcome` be tested against a captured real response
    /// without a live endpoint.
    fn from_sse(data: &'static str) -> ChatStream {
        let reader: Box<dyn Read + Send + Sync> = Box::new(data.as_bytes());
        ChatStream {
            lines: BufReader::new(reader).lines(),
            started: Instant::now(),
            content: String::new(),
            reasoning: String::new(),
            finish_reason: None,
            usage: Usage::default(),
        }
    }
}

fn field(v: Option<&Value>, key: &str) -> u64 {
    v.and_then(|v| v.get(key))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// Applies the hard character cap client-side. Deterministic and independent
/// of whatever the model actually did — this is what makes `max_chars` a real
/// guarantee rather than a hint the model can ignore.
pub fn enforce_max_chars(text: &str, max_chars: Option<usize>) -> (String, bool) {
    match max_chars {
        Some(n) if text.chars().count() > n => {
            let truncated: String = text.chars().take(n).collect();
            (truncated, true)
        }
        _ => (text.to_string(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Effort, Settings};

    #[test]
    fn token_budget_is_sent_and_explained_to_model() {
        let settings = Settings {
            budget_tokens: Some(8192),
            ..Settings::default()
        };
        let body = build_body("model", &settings, "", &[ChatMessage::user("hi")], None);
        assert_eq!(body["max_tokens"], 8192);
        let system = body["messages"][0]["content"].as_str().unwrap();
        assert!(system.contains("entire generation"));
        assert!(system.contains("8192 tokens"));
    }

    #[test]
    fn effort_none_disables_thinking_switches() {
        let settings = Settings::default(); // Effort::None
        let body = build_body("model", &settings, "", &[ChatMessage::user("hi")], None);
        assert_eq!(body["reasoning_effort"], "none");
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
    }

    #[test]
    fn effort_high_passes_through_without_forcing_thinking_off() {
        let settings = Settings {
            effort: Effort::High,
            ..Settings::default()
        };
        let body = build_body("model", &settings, "", &[ChatMessage::user("hi")], None);
        assert_eq!(body["reasoning_effort"], "high");
        assert!(body.get("chat_template_kwargs").is_none());
    }

    #[test]
    fn history_round_trips_in_order() {
        let settings = Settings::default();
        let history = vec![
            ChatMessage::user("first"),
            ChatMessage::assistant("reply"),
            ChatMessage::user("second"),
        ];
        let body = build_body("model", &settings, "", &history, None);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[2]["content"], "second");
    }

    #[test]
    fn max_chars_truncates_deterministically() {
        let (out, cut) = enforce_max_chars("hello world", Some(5));
        assert_eq!(out, "hello");
        assert!(cut);
        let (out, cut) = enforce_max_chars("hi", Some(5));
        assert_eq!(out, "hi");
        assert!(!cut);
    }

    /// Captured live from the Yolo-Auto endpoint (see AGENTS.md live
    /// verification) with `reasoning_effort: "none"` — exercises the exact
    /// wire shape `next_chunk`/`into_outcome` need to handle: an empty
    /// role-priming delta, multi-token content deltas, a finish-only delta,
    /// a usage-only trailing chunk with empty `choices`, then `[DONE]`.
    const SAMPLE_SSE: &str = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":null,\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":null,\"content\":\"1\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":null,\"content\":\"\\n2\\n3\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":null},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":25,\"completion_tokens\":10,\"total_tokens\":35}}\n\n",
        "data: [DONE]\n",
    );

    #[test]
    fn stream_accumulates_deltas_in_order() {
        let mut stream = ChatStream::from_sse(SAMPLE_SSE);
        let mut pieces = Vec::new();
        while let Some(piece) = stream.next_chunk().unwrap() {
            pieces.push(piece);
        }
        assert_eq!(pieces, vec!["1", "\n2\n3"]);
    }

    #[test]
    fn stream_into_outcome_matches_the_non_streaming_shape() {
        let mut stream = ChatStream::from_sse(SAMPLE_SSE);
        while stream.next_chunk().unwrap().is_some() {}
        let outcome = stream.into_outcome();
        assert_eq!(outcome.text(), "1\n2\n3");
        assert_eq!(outcome.finish_reason.as_deref(), Some("stop"));
        assert_eq!(outcome.usage.prompt_tokens, 25);
        assert_eq!(outcome.usage.completion_tokens, 10);
        assert_eq!(outcome.usage.total_tokens, 35);
        assert!(outcome.stopped_by_sequence());
    }

    #[test]
    fn stream_empty_body_yields_no_content() {
        let mut stream = ChatStream::from_sse("data: [DONE]\n");
        assert!(stream.next_chunk().unwrap().is_none());
        let outcome = stream.into_outcome();
        assert!(outcome.content.is_none());
        assert_eq!(outcome.text(), "");
    }

    #[test]
    fn json_mode_sends_strict_schema() {
        let settings = Settings {
            json_mode: crate::config::JsonMode {
                enabled: true,
                schema: json!({"type": "object"}),
            },
            ..Settings::default()
        };
        let schema = json!({"type": "object", "properties": {"title": {"type": "string"}}});
        let body = build_body(
            "model",
            &settings,
            "",
            &[ChatMessage::user("hi")],
            Some(&schema),
        );
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        assert_eq!(
            body["response_format"]["json_schema"]["schema"]["properties"]["title"]["type"],
            "string"
        );
    }
}
