//! Talking to the OpenAI-compatible endpoint. Multi-turn: the whole visible
//! history is resent every call, exactly like a real chat client.

use serde::Deserialize;
use serde_json::{json, Value};
use std::env;
use std::fs;
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
