//! Thin z.ai transport: assemble the OpenAI-compatible body, POST / stream
//! `/chat/completions`, parse the response. Conversation policy lives in
//! `agent.rs`. The coding-plan base (`/api/coding/paas/v4`) is never used.

use serde_json::{json, Value};
use crate::auth::{self, Provider};
use crate::config::{self, Res, Settings, DEFAULT_MODEL};
use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;



/// Plain (non-coding-plan) OpenAI-compatible base.
pub const DEFAULT_BASE_URL: &str = "https://api.z.ai/api/paas/v4";

/// Only this id may be sent on a live completion. The rest of
/// [`config::MODEL_CATALOG`] is selectable but expensive.
pub const LIVE_COMPLETION_MODEL: &str = DEFAULT_MODEL;

#[derive(Clone)]
pub struct Endpoint {
    pub provider: Provider,
    pub base_url: String,
    api_key: String,
}

impl Endpoint {
    /// Carries no key and points nowhere, so any request through it fails.
    /// Used by code paths that must provably not reach the network — tests,
    /// the offline half of the isolation proof, and the keyless TUI boot
    /// before `/login` connects a key.
    pub fn unusable() -> Endpoint {
        Endpoint {
            provider: Provider::Glm,
            base_url: "http://unused.invalid".into(),
            api_key: String::new(),
        }
    }

    #[cfg(test)]
    pub fn dummy() -> Endpoint {
        Endpoint::unusable()
    }

    pub fn has_key(&self) -> bool {
        !self.api_key.is_empty()
    }

    /// Default endpoint: z.ai with the glm key. Kept so existing call sites
    /// and tests keep their "glm by default" behavior; `for_provider` is
    /// the explicit form.
    pub fn resolve() -> Res<Endpoint> {
        Endpoint::for_provider(Provider::Glm)
    }

    /// Resolves the endpoint from the user's own machine: `$ENV` →
    /// `~/.ask6/auth.json`. See `auth::resolve`.
    pub fn for_provider(provider: Provider) -> Res<Endpoint> {
        let resolved = auth::resolve(provider)?;
        Ok(Endpoint {
            provider,
            base_url: resolved.base_url,
            api_key: resolved.key,
        })
    }

    /// Resolves the endpoint that owns `model` — how `--model deepseek-flash`
    /// actually reaches DeepSeek. Unknown ids get the same error `--model`
    /// produces; a known id whose provider has no key errors like any
    /// unresolved provider.
    pub fn for_model(model: &str) -> Res<Endpoint> {
        let provider = config::provider_of(model).ok_or_else(|| config::catalog_error(model))?;
        Endpoint::for_provider(provider)
    }
}



/// The money guard as a predicate — same rule as [`guard_live_model`],
/// usable where a `bool` is handier than a `Result`.
pub fn is_live_model(provider: Provider, model: &str) -> bool {
    guard_live_model(provider, model).is_ok()
}


/// Per-provider money guard: glm and DeepSeek permit their flash tiers;
/// OpenRouter permits `:free` ids. Everything else stays selectable but is
/// refused at send time.
pub fn guard_live_model(provider: Provider, model: &str) -> Res<()> {
    let allowed = match provider {
        Provider::Glm => model == LIVE_COMPLETION_MODEL,
        Provider::DeepSeek => model == "deepseek-flash",
        Provider::OpenRouter => model.ends_with(":free"),
    };
    if !allowed {
        let what = match provider {
            Provider::Glm => format!("only `{LIVE_COMPLETION_MODEL}`"),
            Provider::DeepSeek => "only `deepseek-flash`".to_string(),
            Provider::OpenRouter => "only ids ending in `:free`".to_string(),
        };
        return Err(format!(
            "refusing to call `{model}` on {}: {what} may be used for live \
completions (other catalog models are expensive)",
            provider.id()
        ));
    }
    Ok(())
}

/// Live `GET {base}/models` for a connected endpoint. Used by `ask --models --live`
/// to report catalog drift — never to auto-edit the catalog.
pub fn list_model_ids(ep: &Endpoint) -> Res<Vec<String>> {
    let url = format!("{}/models", ep.base_url.trim_end_matches('/'));
    let resp = ureq::get(&url)
        .set("Authorization", &format!("Bearer {}", ep.api_key))
        .set("Accept-Language", "en-US,en")
        .call();
    let raw = read_json_response(resp, &url)?;
    let Some(data) = raw.get("data").and_then(|d| d.as_array()) else {
        return Err(format!(
            "unexpected /models shape from {}: missing data[]",
            ep.provider.id()
        ));
    };
    Ok(data
        .iter()
        .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(str::to_string))
        .collect())
}


#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    pub fn as_str(self) -> &'static str {
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
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Clone, Debug)]
pub struct Outcome {
    pub content: Option<String>,
    pub reasoning: Option<String>,
    pub finish_reason: Option<String>,
    /// Model id the provider echoed (`glm-5.3-flash` live).
    pub model: Option<String>,
    pub usage: Usage,
    pub raw: Value,
    pub latency_ms: u128,
}

impl Outcome {
    pub fn truncated(&self) -> bool {
        matches!(self.finish_reason.as_deref(), Some("length"))
    }

    pub fn stopped_by_sequence(&self) -> bool {
        matches!(self.finish_reason.as_deref(), Some("stop"))
    }

    pub fn text(&self) -> &str {
        self.content.as_deref().unwrap_or("").trim()
    }
}

/// Builds the request body. Kept separate from `chat` so tests can inspect
/// it without a network call. The agent decides *what* goes in; this just
/// serializes the settings onto the provider's parameter names. glm and
/// DeepSeek both accept `thinking` and `reasoning_effort`; OpenRouter's own
/// `reasoning` block is deliberately not invented here.
pub fn build_body(
    provider: Provider,
    model: &str,
    settings: &Settings,
    system: &str,
    history: &[ChatMessage],
    schema: Option<&Value>,
) -> Value {
    let mut settings = settings.clone();
    settings.clamp();

    let mut messages = Vec::new();
    let mut system = system.to_string();

    if let Some(n) = settings.max_tokens() {
        let note = format!(
            "The entire generation, including reasoning and the final answer, has a hard budget of {n} tokens. Plan accordingly and finish the final answer before that limit; never stop mid-answer."
        );
        append_system(&mut system, &note);
    }
    if let Some(n) = settings.max_chars {
        append_system(
            &mut system,
            &format!("Keep your entire final answer under {n} characters."),
        );
    }
    if settings.json_mode.enabled {
        let schema = schema
            .cloned()
            .unwrap_or_else(|| settings.json_mode.schema.clone());
        // Live: `response_format.json_schema` was HTTP 200 but not enforced
        // (markdown fences + extra fields). Ask in the prompt instead.
        append_system(
            &mut system,
            &format!(
                "Reply with a single JSON object matching this schema, no markdown fences:\n{schema}"
            ),
        );
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
    if matches!(provider, Provider::Glm | Provider::DeepSeek) {
        if let Some(obj) = body.as_object_mut() {
            let thinking = if provider == Provider::Glm {
                // glm-5.3-flash documents clear_thinking and cannot disable
                // thinking. DeepSeek accepts only the portable `type` field.
                json!({ "type": "enabled", "clear_thinking": false })
            } else {
                json!({ "type": "enabled" })
            };
            obj.insert("thinking".into(), thinking);
            obj.insert("reasoning_effort".into(), json!(settings.effort.wire()));
        }
    }
    if let Some(obj) = body.as_object_mut() {
        if settings.json_mode.enabled {
            obj.insert(
                "response_format".into(),
                json!({ "type": "json_object" }),
            );
        }
        if let Some(n) = settings.max_tokens() {
            obj.insert("max_tokens".into(), json!(n));
        }
        if !settings.stop.is_empty() {
            obj.insert("stop".into(), json!(settings.stop));
        }
        if let Some(t) = settings.temperature {
            obj.insert("temperature".into(), json!(t));
        }
        if let Some(p) = settings.top_p {
            obj.insert("top_p".into(), json!(p));
        }
        if let Some(k) = settings.top_k {
            obj.insert("top_k".into(), json!(k));
        }
    }
    body
}

fn append_system(system: &mut String, note: &str) {
    if system.is_empty() {
        *system = note.to_string();
    } else {
        system.push_str("\n\n");
        system.push_str(note);
    }
}

pub fn chat(
    ep: &Endpoint,
    settings: &Settings,
    system: &str,
    history: &[ChatMessage],
    schema: Option<&Value>,
) -> Res<Outcome> {
    guard_live_model(ep.provider, &settings.model)?;
    let body = build_body(ep.provider, &settings.model, settings, system, history, schema);
    post_completion(ep, body)
}

fn post_completion(ep: &Endpoint, body: Value) -> Res<Outcome> {
    let url = format!("{}/chat/completions", ep.base_url);
    let started = Instant::now();
    let resp = ureq::post(&url)
        .set("Authorization", &format!("Bearer {}", ep.api_key))
        .set("Content-Type", "application/json")
        .set("Accept-Language", "en-US,en")
        .send_json(body);
    let latency_ms = started.elapsed().as_millis();
    let raw = read_json_response(resp, &url)?;
    parse_outcome(raw, latency_ms)
}

pub fn parse_outcome(raw: Value, latency_ms: u128) -> Res<Outcome> {
    if let Some(err) = raw.get("error") {
        return Err(format_error_value(err));
    }
    let choice = raw
        .get("choices")
        .and_then(|c| c.get(0))
        .ok_or_else(|| format!("no choices in response: {raw}"))?;
    let message = choice.get("message");
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
    let model = raw
        .get("model")
        .and_then(|m| m.as_str())
        .map(str::to_string);
    let u = raw.get("usage");
    Ok(Outcome {
        content,
        reasoning,
        finish_reason,
        model,
        usage: usage_from(u),
        raw,
        latency_ms,
    })
}

fn usage_from(u: Option<&Value>) -> Usage {
    Usage {
        prompt_tokens: field(u, "prompt_tokens"),
        completion_tokens: field(u, "completion_tokens"),
        reasoning_tokens: u
            .and_then(|u| u.get("completion_tokens_details"))
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or_else(|| field(u, "reasoning_tokens")),
        total_tokens: field(u, "total_tokens"),
    }
}

pub fn format_http_error(code: u16, url: &str, body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        if let Some(err) = v.get("error") {
            return format!("HTTP {code} from {url}: {}", format_error_value(err));
        }
    }
    format!("HTTP {code} from {url}: {}", body.trim())
}

fn format_error_value(err: &Value) -> String {
    let msg = err
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    match err.get("code") {
        Some(Value::String(c)) if !c.is_empty() => format!("[{c}] {msg}"),
        Some(Value::Number(n)) => format!("[{n}] {msg}"),
        _ if !msg.is_empty() => msg.to_string(),
        _ => err.to_string(),
    }
}

fn read_json_response(resp: Result<ureq::Response, ureq::Error>, url: &str) -> Res<Value> {
    let resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let detail = r.into_string().unwrap_or_default();
            return Err(format_http_error(code, url, &detail));
        }
        Err(e) => return Err(format!("request failed: {e}")),
    };
    resp.into_json()
        .map_err(|e| format!("response was not JSON: {e}"))
}

pub struct ChatStream {
    lines: std::io::Lines<BufReader<Box<dyn Read + Send + Sync>>>,
    started: Instant,
    content: String,
    reasoning: String,
    finish_reason: Option<String>,
    model: Option<String>,
    usage: Usage,
    cancel: Option<Arc<AtomicBool>>,
}

impl ChatStream {
    pub fn next_chunk(&mut self) -> Res<Option<String>> {
        loop {
            if self
                .cancel
                .as_ref()
                .is_some_and(|c| c.load(Ordering::Relaxed))
            {
                return Ok(None);
            }
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

            if let Some(m) = chunk.get("model").and_then(Value::as_str) {
                self.model = Some(m.to_string());
            }
            if let Some(u) = chunk.get("usage").filter(|u| !u.is_null()) {
                self.usage = usage_from(Some(u));
            }
            if let Some(err) = chunk.get("error") {
                return Err(format_error_value(err));
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

    pub fn into_outcome(self) -> Outcome {
        Outcome {
            content: (!self.content.is_empty()).then_some(self.content),
            reasoning: (!self.reasoning.is_empty()).then_some(self.reasoning),
            finish_reason: self.finish_reason,
            model: self.model,
            usage: self.usage,
            raw: Value::Null,
            latency_ms: self.started.elapsed().as_millis(),
        }
    }
}

pub fn chat_stream(
    ep: &Endpoint,
    settings: &Settings,
    system: &str,
    history: &[ChatMessage],
    schema: Option<&Value>,
    cancel: Option<Arc<AtomicBool>>,
) -> Res<ChatStream> {
    guard_live_model(ep.provider, &settings.model)?;
    let mut body = build_body(ep.provider, &settings.model, settings, system, history, schema);
    if let Some(obj) = body.as_object_mut() {
        obj.insert("stream".into(), json!(true));
        obj.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    let url = format!("{}/chat/completions", ep.base_url);
    let started = Instant::now();
    let resp = ureq::post(&url)
        .set("Authorization", &format!("Bearer {}", ep.api_key))
        .set("Content-Type", "application/json")
        .set("Accept-Language", "en-US,en")
        .send_json(body);
    let resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let detail = r.into_string().unwrap_or_default();
            return Err(format_http_error(code, url.as_str(), &detail));
        }
        Err(e) => return Err(format!("request failed: {e}")),
    };
    Ok(ChatStream {
        lines: BufReader::new(resp.into_reader()).lines(),
        started,
        content: String::new(),
        reasoning: String::new(),
        finish_reason: None,
        model: None,
        usage: Usage::default(),
        cancel,
    })
}

#[cfg(test)]
impl ChatStream {
    fn from_sse(data: &'static str) -> ChatStream {
        let reader: Box<dyn Read + Send + Sync> = Box::new(data.as_bytes());
        ChatStream {
            lines: BufReader::new(reader).lines(),
            started: Instant::now(),
            content: String::new(),
            reasoning: String::new(),
            finish_reason: None,
            model: None,
            usage: Usage::default(),
            cancel: None,
        }
    }
}

fn field(v: Option<&Value>, key: &str) -> u64 {
    v.and_then(|v| v.get(key))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

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

    fn body_for(settings: &Settings) -> Value {
        build_body(
            Provider::Glm,
            &settings.model,
            settings,
            &settings.system_prompt,
            &[ChatMessage::user("hi")],
            None,
        )
    }

    #[test]
    fn thinking_fields_are_sent_to_both_direct_providers() {
        let settings = Settings::default();
        let ds = build_body(
            Provider::DeepSeek,
            "deepseek-flash",
            &settings,
            "",
            &[ChatMessage::user("hi")],
            None,
        );
        assert_eq!(ds["thinking"]["type"], "enabled");
        assert!(ds["thinking"].get("clear_thinking").is_none());
        assert_eq!(ds["reasoning_effort"], "low");
        assert_eq!(ds["model"], "deepseek-flash");

        let or = build_body(
            Provider::OpenRouter,
            "google/gemma-4-31b-it:free",
            &settings,
            "",
            &[ChatMessage::user("hi")],
            None,
        );
        assert!(or.get("thinking").is_none());
        assert!(or.get("reasoning_effort").is_none());

        let glm = body_for(&settings);
        assert_eq!(glm["thinking"]["type"], "enabled");
        assert_eq!(glm["thinking"]["clear_thinking"], false);
        assert!(glm.get("reasoning_effort").is_some());
    }

    #[test]
    fn token_budget_is_sent_as_max_tokens() {
        let settings = Settings {
            budget_tokens: Some(8192),
            ..Settings::default()
        };
        let body = body_for(&settings);
        assert_eq!(body["max_tokens"], 8192);
        let system = body["messages"][0]["content"].as_str().unwrap();
        assert!(system.contains("8192 tokens"));
    }

    #[test]
    fn thinking_is_always_enabled_with_legal_effort() {
        let body = body_for(&Settings::default());
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["clear_thinking"], false);
        assert_eq!(body["reasoning_effort"], "low");
        assert!(body.get("chat_template_kwargs").is_none());
    }

    #[test]
    fn effort_max_is_sent_as_max() {
        let settings = Settings {
            effort: Effort::Max,
            ..Settings::default()
        };
        let body = body_for(&settings);
        assert_eq!(body["reasoning_effort"], "max");
        assert_eq!(body["thinking"]["type"], "enabled");
    }

    #[test]
    fn legacy_none_and_medium_are_rewritten_before_the_wire() {
        let none = Settings {
            effort: Effort::None,
            ..Settings::default()
        };
        assert_eq!(body_for(&none)["reasoning_effort"], "low");
        let med = Settings {
            effort: Effort::Medium,
            ..Settings::default()
        };
        assert_eq!(body_for(&med)["reasoning_effort"], "high");
    }

    #[test]
    fn sampling_knobs_are_sent_only_when_set() {
        let plain = body_for(&Settings::default());
        assert!(plain.get("temperature").is_none());
        assert!(plain.get("top_p").is_none());
        assert!(plain.get("top_k").is_none());

        let settings = Settings {
            temperature: Some(0.7),
            top_p: Some(0.95),
            top_k: Some(-1),
            ..Settings::default()
        };
        let body = body_for(&settings);
        assert!((body["temperature"].as_f64().unwrap() - 0.7).abs() < 1e-6);
        assert!((body["top_p"].as_f64().unwrap() - 0.95).abs() < 1e-5);
        assert_eq!(body["top_k"], -1);
    }

    #[test]
    fn temperature_above_docs_range_is_clamped_not_sent_raw() {
        let settings = Settings {
            temperature: Some(2.0),
            ..Settings::default()
        };
        let body = body_for(&settings);
        assert!((body["temperature"].as_f64().unwrap() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn history_round_trips_in_order() {
        let settings = Settings::default();
        let history = vec![
            ChatMessage::user("first"),
            ChatMessage::assistant("reply"),
            ChatMessage::user("second"),
        ];
        let body = build_body(Provider::Glm, "glm-5.3-flash", &settings, "", &history, None);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "first");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[2]["content"], "second");
    }

    #[test]
    fn default_system_prompt_is_the_first_message() {
        let body = body_for(&Settings::default());
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(
            body["messages"][0]["content"],
            crate::config::DEFAULT_SYSTEM_PROMPT
        );
    }

    #[test]
    fn json_mode_sends_json_object_not_json_schema() {
        let settings = Settings {
            json_mode: crate::config::JsonMode {
                enabled: true,
                schema: json!({"type": "object"}),
            },
            ..Settings::default()
        };
        let schema = json!({"type": "object", "properties": {"title": {"type": "string"}}});
        let body = build_body(
            Provider::Glm,
            "glm-5.3-flash",
            &settings,
            "",
            &[ChatMessage::user("hi")],
            Some(&schema),
        );
        assert_eq!(body["response_format"]["type"], "json_object");
        assert!(body["response_format"].get("json_schema").is_none());
        let system = body["messages"][0]["content"].as_str().unwrap();
        assert!(system.contains("title"));
    }

    #[test]
    fn model_field_is_the_settings_model() {
        let settings = Settings::default();
        let body = build_body(Provider::Glm, "glm-5.3-flash", &settings, "", &[ChatMessage::user("hi")], None);
        assert_eq!(body["model"], "glm-5.3-flash");
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
    fn parse_outcome_reads_model_usage_and_reasoning() {
        let raw = json!({
            "model": "glm-5.3-flash",
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": "OK",
                    "reasoning_content": "brief thought"
                }
            }],
            "usage": {
                "prompt_tokens": 19,
                "completion_tokens": 19,
                "total_tokens": 38,
                "completion_tokens_details": { "reasoning_tokens": 16 }
            }
        });
        let out = parse_outcome(raw, 12).unwrap();
        assert_eq!(out.model.as_deref(), Some("glm-5.3-flash"));
        assert_eq!(out.text(), "OK");
        assert_eq!(out.finish_reason.as_deref(), Some("stop"));
        assert_eq!(out.usage.prompt_tokens, 19);
        assert_eq!(out.usage.completion_tokens, 19);
        assert_eq!(out.usage.reasoning_tokens, 16);
        assert_eq!(out.usage.total_tokens, 38);
        assert_eq!(out.reasoning.as_deref(), Some("brief thought"));
        assert_eq!(out.raw["model"], "glm-5.3-flash");
    }

    #[test]
    fn parse_outcome_surfaces_error_payloads() {
        let raw = json!({
            "error": { "code": "1210", "message": "This model always engages in thinking and cannot be disabled; please use low, high, or max" }
        });
        let err = parse_outcome(raw, 0).unwrap_err();
        assert!(err.contains("1210"));
        assert!(err.contains("cannot be disabled"));
    }

    #[test]
    fn format_http_error_reads_z_ai_error_object() {
        let body = r#"{"error":{"code":"1210","message":"please use low, high, or max"}}"#;
        let s = format_http_error(400, "https://api.z.ai/api/paas/v4/chat/completions", body);
        assert!(s.contains("HTTP 400"));
        assert!(s.contains("1210"));
        assert!(s.contains("please use low, high, or max"));
    }

    #[test]
    fn guard_rejects_expensive_catalog_models() {
        assert!(guard_live_model(Provider::Glm, "glm-5.3-flash").is_ok());
        let err = guard_live_model(Provider::Glm, "glm-5.3").unwrap_err();
        assert!(err.contains("glm-5.3-flash"));
        assert!(err.contains("expensive"));
    }

    #[test]
    fn guard_is_provider_specific() {
        assert!(guard_live_model(Provider::DeepSeek, "deepseek-flash").is_ok());
        let err = guard_live_model(Provider::DeepSeek, "deepseek-v4-pro").unwrap_err();
        assert!(err.contains("deepseek-flash"));
        assert!(err.contains("expensive"));
        assert!(guard_live_model(Provider::OpenRouter, "meta-llama/llama-3.3-70b-instruct:free").is_ok());
        assert!(guard_live_model(Provider::OpenRouter, "openai/gpt-4o").is_err());
    }

    /// The catalog's `live` flags and the money guard are one rule, not two.
    /// This is the guard against a pasted paid id quietly becoming callable.
    #[test]
    fn catalog_live_flags_match_the_money_guard_exactly() {
        for m in config::MODEL_CATALOG {
            assert_eq!(
                m.live,
                is_live_model(m.provider, m.id),
                "catalog `{}` (live={}) disagrees with the money guard",
                m.id,
                m.live
            );
        }
        // …and a paid id that merely *looks* catalog-adjacent stays refused.
        assert!(!is_live_model(Provider::OpenRouter, "z-ai/glm-5.3"));
        assert!(!is_live_model(Provider::OpenRouter, "openai/gpt-5"));
    }


    const SAMPLE_SSE: &str = concat!(
        "data: {\"model\":\"glm-5.3-flash\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":null,\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n",
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
        assert_eq!(outcome.model.as_deref(), Some("glm-5.3-flash"));
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
    fn stream_cancel_flag_stops_the_read_loop() {
        let mut stream = ChatStream::from_sse(SAMPLE_SSE);
        stream.cancel = Some(Arc::new(AtomicBool::new(true)));
        assert_eq!(stream.next_chunk(), Ok(None));
    }

    #[test]
    fn coding_plan_base_is_not_the_default() {
        assert_eq!(DEFAULT_BASE_URL, "https://api.z.ai/api/paas/v4");
        assert!(!DEFAULT_BASE_URL.contains("/coding/"));
    }
}
