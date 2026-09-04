//! Клиент к OpenAI-совместимым endpoint'ам.
//!
//! Провайдеров два, и это не про «на всякий случай»: подписочный endpoint Z.AI
//! молча игнорирует `temperature` (см. `verify.rs` — там это доказывается
//! замером, а не на словах), поэтому для самого эксперимента нужен бэкенд,
//! который параметр честно применяет. Разбор при этом всё равно делает
//! `glm-5.3` — на него игнор температуры не влияет.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub type Res<T> = Result<T, String>;

/// Разбор трёх ответов — всегда старшая модель Z.AI, как в задании.
pub const JUDGE_PROVIDER: Provider = Provider::Zai;
pub const JUDGE_MODEL: &str = "glm-5.3";

/// Конкретная площадка OpenRouter, к которой прибиты все прогоны (см. `adjust`).
///
/// Выбрана не по цене: `deepinfra/turbo` при temperature=0 выдавал разные
/// ответы на один и тот же запрос (спекулятивное декодирование ломает
/// воспроизводимость жадного прохода), и это выглядело как «температура
/// не работает». `nebius/fp8`, `crusoe/bf16` и `novita/bf16` дают 4/4
/// одинаковых ответа; берём первую — дешёвую и со 131k контекста.
const OPENROUTER_BACKEND: &str = "nebius/fp8";

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// Coding-подписка Z.AI из `~/.pi/agent/auth.json`.
    Zai,
    /// Ключ OpenRouter оттуда же.
    OpenRouter,
}

impl Provider {
    pub const ALL: [Provider; 2] = [Provider::OpenRouter, Provider::Zai];

    pub fn id(self) -> &'static str {
        match self {
            Provider::Zai => "zai",
            Provider::OpenRouter => "openrouter",
        }
    }

    pub fn parse(s: &str) -> Res<Provider> {
        match s {
            "zai" => Ok(Provider::Zai),
            "openrouter" | "or" => Ok(Provider::OpenRouter),
            other => Err(format!("неизвестный провайдер: {other} (zai | openrouter)")),
        }
    }

    fn default_base_url(self) -> &'static str {
        match self {
            Provider::Zai => "https://open.bigmodel.cn/api/coding/paas/v4",
            Provider::OpenRouter => "https://openrouter.ai/api/v1",
        }
    }

    /// Модель, которой задаём три температуры.
    pub fn answer_model(self) -> &'static str {
        match self {
            Provider::Zai => "glm-5.3-flash",
            Provider::OpenRouter => "meta-llama/llama-3.3-70b-instruct",
        }
    }

    /// Ключ в `~/.pi/agent/auth.json` и переменная окружения, которая его перебивает.
    fn auth_entry(self) -> (&'static str, &'static str) {
        match self {
            Provider::Zai => ("zai-coding-cn", "ZAI_API_KEY"),
            Provider::OpenRouter => ("openrouter", "OPENROUTER_API_KEY"),
        }
    }

    /// Правки тела запроса под конкретный API.
    fn adjust(self, body: &mut Value, thinking: bool) {
        match self {
            // GLM по умолчанию «думает», а длинная цепочка рассуждений сама по
            // себе усредняет выдачу: финальный текст пересказывает вывод, а не
            // сэмплируется свободно. Для чистоты эксперимента её глушим.
            Provider::Zai => {
                if !thinking {
                    body["thinking"] = json!({ "type": "disabled" });
                }
            }
            // Два обязательных условия, иначе эксперимент нечист:
            // `require_parameters` — не уходить на бэкенд, который проглотит
            // temperature молча; `order` + запрет фолбэка — держать все прогоны
            // на одном и том же бэкенде. Без пиннинга OpenRouter балансирует
            // между площадками с разной квантизацией, и temperature=0 перестаёт
            // быть воспроизводимой по причинам, к температуре не относящимся.
            Provider::OpenRouter => {
                body["provider"] = json!({
                    "require_parameters": true,
                    "order": [OPENROUTER_BACKEND],
                    "allow_fallbacks": false,
                });
            }
        }
    }
}

#[derive(Clone)]
pub struct Client {
    pub provider: Provider,
    pub base_url: String,
    api_key: String,
}

impl Client {
    /// Только для тестов, которые обязаны вернуться до сетевого вызова.
    #[cfg(test)]
    pub fn dummy() -> Client {
        Client {
            provider: Provider::Zai,
            base_url: "http://unused.invalid".into(),
            api_key: String::new(),
        }
    }

    pub fn new(provider: Provider) -> Res<Client> {
        let (_, env_var) = provider.auth_entry();
        Ok(Client {
            base_url: env::var(format!("{env_var}_BASE_URL"))
                .unwrap_or_else(|_| provider.default_base_url().into()),
            api_key: resolve_api_key(provider)?,
            provider,
        })
    }

    pub fn complete(&self, req: &Request) -> Res<Reply> {
        let mut body = json!({
            "model": req.model,
            "messages": messages_of(req),
            "max_tokens": req.max_tokens,
        });
        // Ровно тот рычаг, который изучаем. `None` — поле не отправляем вовсе,
        // чтобы «дефолт сервера» можно было замерить отдельным случаем.
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        self.provider.adjust(&mut body, req.thinking);

        let started = Instant::now();
        let resp = ureq::post(&format!("{}/chat/completions", self.base_url))
            .set("Authorization", &format!("Bearer {}", self.api_key))
            .set("Content-Type", "application/json")
            .timeout(Duration::from_secs(300))
            .send_json(body);
        let latency_ms = started.elapsed().as_millis() as u64;

        let text = match resp {
            Ok(r) => r.into_string().map_err(|e| format!("чтение ответа: {e}"))?,
            Err(ureq::Error::Status(code, r)) => {
                let detail = r.into_string().unwrap_or_default();
                return Err(format!("HTTP {code}: {}", truncate(&detail, 400)));
            }
            Err(e) => return Err(format!("сеть: {e}")),
        };

        let parsed: ApiResponse = serde_json::from_str(&text)
            .map_err(|e| format!("нераспознанный JSON ({e}): {}", truncate(&text, 300)))?;
        // OpenRouter отдаёт ошибку с HTTP 200 — молча вернуть пустой ответ хуже,
        // чем упасть с текстом причины.
        if let Some(err) = parsed.error {
            return Err(format!("{}: {}", req.model, truncate(&err.message, 300)));
        }
        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| format!("{}: ответ без choices", req.model))?;
        let usage = parsed.usage.unwrap_or_default();

        Ok(Reply {
            content: choice.message.content.unwrap_or_default().trim().to_string(),
            finish_reason: choice.finish_reason.unwrap_or_else(|| "?".into()),
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            reasoning_tokens: usage
                .completion_tokens_details
                .map(|d| d.reasoning_tokens)
                .unwrap_or(0),
            latency_ms,
        })
    }
}

fn messages_of(req: &Request) -> Value {
    let mut out = Vec::new();
    if let Some(sys) = &req.system {
        out.push(json!({ "role": "system", "content": sys }));
    }
    out.push(json!({ "role": "user", "content": req.prompt }));
    Value::Array(out)
}

fn resolve_api_key(provider: Provider) -> Res<String> {
    let (entry, env_var) = provider.auth_entry();
    if let Ok(k) = env::var(env_var) {
        if !k.trim().is_empty() {
            return Ok(k);
        }
    }
    let path = auth_path()?;
    let raw = fs::read_to_string(&path).map_err(|e| format!("не читается {}: {e}", path.display()))?;
    let auth: Value =
        serde_json::from_str(&raw).map_err(|e| format!("не парсится {}: {e}", path.display()))?;
    auth.get(entry)
        .and_then(|p| p.get("key"))
        .and_then(Value::as_str)
        .filter(|k| !k.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("нет ключа {entry}.key в {} и пустой ${env_var}", path.display()))
}

fn auth_path() -> Res<PathBuf> {
    env::var("HOME")
        .map(|h| PathBuf::from(h).join(".pi/agent/auth.json"))
        .map_err(|_| "HOME не задан".to_string())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n).collect::<String>() + "…"
}

#[derive(Clone, Debug)]
pub struct Request {
    pub model: String,
    pub system: Option<String>,
    pub prompt: String,
    /// `None` — поле `temperature` не уходит в запрос вообще.
    pub temperature: Option<f64>,
    pub max_tokens: u32,
    /// Для Z.AI: `false` просит модель не рассуждать (см. `Provider::adjust`).
    pub thinking: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Reply {
    pub content: String,
    pub finish_reason: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub reasoning_tokens: u32,
    pub latency_ms: u64,
}

#[derive(Deserialize)]
struct ApiResponse {
    #[serde(default)]
    choices: Vec<Choice>,
    usage: Option<Usage>,
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct ApiError {
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct Message {
    content: Option<String>,
}

#[derive(Deserialize, Default)]
struct Usage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    completion_tokens_details: Option<Details>,
}

#[derive(Deserialize)]
struct Details {
    #[serde(default)]
    reasoning_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(temperature: Option<f64>, thinking: bool) -> Request {
        Request {
            model: "m".into(),
            system: Some("sys".into()),
            prompt: "hi".into(),
            temperature,
            max_tokens: 10,
            thinking,
        }
    }

    #[test]
    fn system_message_goes_first() {
        let m = messages_of(&req(Some(0.7), false));
        assert_eq!(m[0]["role"], "system");
        assert_eq!(m[1]["role"], "user");
        assert_eq!(m[1]["content"], "hi");
    }

    #[test]
    fn no_system_means_single_user_message() {
        let mut r = req(None, false);
        r.system = None;
        let m = messages_of(&r);
        assert_eq!(m.as_array().unwrap().len(), 1);
        assert_eq!(m[0]["role"], "user");
    }

    #[test]
    fn zai_disables_thinking_only_when_asked() {
        let mut on = json!({});
        Provider::Zai.adjust(&mut on, true);
        assert!(on.get("thinking").is_none());

        let mut off = json!({});
        Provider::Zai.adjust(&mut off, false);
        assert_eq!(off["thinking"]["type"], "disabled");
    }

    /// Все три температуры обязаны попасть на один бэкенд, иначе разброс при
    /// temperature=0 объясняется маршрутизацией, а не сэмплингом.
    #[test]
    fn openrouter_pins_one_backend_that_supports_the_parameters() {
        let mut body = json!({});
        Provider::OpenRouter.adjust(&mut body, false);
        assert_eq!(body["provider"]["require_parameters"], true);
        assert_eq!(body["provider"]["allow_fallbacks"], false);
        assert_eq!(body["provider"]["order"], json!([OPENROUTER_BACKEND]));
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn provider_parse_roundtrips_and_rejects_junk() {
        for p in Provider::ALL {
            assert_eq!(Provider::parse(p.id()).unwrap(), p);
        }
        assert!(Provider::parse("gpt").is_err());
    }

    #[test]
    fn truncate_counts_chars_not_bytes() {
        assert_eq!(truncate("привет", 3), "при…");
        assert_eq!(truncate("привет", 6), "привет");
    }
}
