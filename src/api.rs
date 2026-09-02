//! Talking to the OpenAI-compatible endpoint, with the response-control knobs applied.

use serde::Deserialize;
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use crate::config::{Format, Res, RunConfig};

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
    pub fn resolve() -> Res<Endpoint> {
        Ok(Endpoint {
            base_url: env::var("YOLO_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.into()),
            model: env::var("YOLO_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into()),
            api_key: resolve_api_key()?,
        })
    }
}

/// API key resolution order: $YOLO_API_KEY, then ~/.pi/agent/models.json (Yolo-Auto provider).
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

#[derive(Clone, Copy, Debug, Default)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Part of `completion_tokens` that the model spent thinking.
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
}

/// One generation, with everything we need to judge whether the controls worked.
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
    /// True when the provider cut us off rather than the model finishing its thought.
    pub fn truncated(&self) -> bool {
        matches!(self.finish_reason.as_deref(), Some("length"))
    }

    pub fn text(&self) -> &str {
        self.content.as_deref().unwrap_or("").trim()
    }
}

/// Builds the request body. Kept separate so `--dry-run` and tests can inspect it.
pub fn build_body(
    model: &str,
    cfg: &RunConfig,
    system: &str,
    user: &str,
    schema: Option<&Value>,
) -> Value {
    let mut messages = Vec::new();
    if !system.is_empty() {
        messages.push(json!({ "role": "system", "content": system }));
    }
    messages.push(json!({ "role": "user", "content": user }));

    let mut body = json!({
        "model": model,
        "messages": messages,
    });
    let obj = body.as_object_mut().expect("object");

    match cfg.format {
        Format::Text => {}
        Format::JsonObject => {
            obj.insert("response_format".into(), json!({ "type": "json_object" }));
        }
        Format::JsonSchema => {
            let schema = schema
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object" }));
            obj.insert(
                "response_format".into(),
                json!({
                    "type": "json_schema",
                    "json_schema": {
                        "name": "answer",
                        "strict": true,
                        "schema": schema,
                    }
                }),
            );
        }
    }

    if let Some(n) = cfg.max_tokens {
        obj.insert("max_tokens".into(), json!(n));
    }
    if !cfg.stop.is_empty() {
        obj.insert("stop".into(), json!(cfg.stop));
    }
    if let Some(t) = cfg.temperature {
        obj.insert("temperature".into(), json!(t));
    }
    if !cfg.thinking {
        // Two independent switches: the OpenAI-style one and the Qwen chat-template one.
        // Both were verified to zero out reasoning_tokens on this provider.
        obj.insert("reasoning_effort".into(), json!("none"));
        obj.insert(
            "chat_template_kwargs".into(),
            json!({ "enable_thinking": false }),
        );
    }
    body
}

pub fn chat(
    ep: &Endpoint,
    cfg: &RunConfig,
    system: &str,
    user: &str,
    schema: Option<&Value>,
) -> Res<Outcome> {
    let body = build_body(&ep.model, cfg, system, user, schema);
    let url = format!("{}/chat/completions", ep.base_url);

    let started = Instant::now();
    let resp = ureq::post(&url)
        .set("Authorization", &format!("Bearer {}", ep.api_key))
        .set("Content-Type", "application/json")
        .send_json(body);
    let latency_ms = started.elapsed().as_millis();

    let resp = match resp {
        Ok(r) => r,
        // Surface the provider's own error text: it is what tells us a parameter is unsupported.
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

    // `content` is null whenever generation stopped before any visible token — a real
    // case here, because max_tokens and stop both apply to the reasoning stream too.
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
            .unwrap_or(0),
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
