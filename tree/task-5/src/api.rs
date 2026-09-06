//! Клиент к OpenAI-совместимым endpoint'ам четырёх разных площадок.
//!
//! Площадки разные не «на всякий случай»: задание требует слабую, среднюю и
//! сильную модель, а они физически живут у разных провайдеров. Поэтому здесь
//! собраны и адреса, и способы аутентификации, и прайс каждой модели — цену
//! иначе неоткуда взять, а «стоимость» — одна из трёх обязательных метрик.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub type Res<T> = Result<T, String>;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// Реселлер-прокси перед open-weights моделями. Ключ лежит в конфиге pi.
    Yolo,
    /// Родной API xAI. Токен — OAuth из входа в аккаунт, у него есть срок.
    Xai,
    /// Роутер; единственный, кто возвращает реально списанную сумму.
    OpenRouter,
    /// Подписка Synthetic на open-weights модели. Здесь живёт судья.
    Synthetic,
}

impl Provider {
    pub fn id(self) -> &'static str {
        match self {
            Provider::Yolo => "yolo",
            Provider::Xai => "xai",
            Provider::OpenRouter => "openrouter",
            Provider::Synthetic => "synthetic",
        }
    }

    fn default_base_url(self) -> &'static str {
        match self {
            Provider::Yolo => "https://yolo-auto.com/v1",
            Provider::Xai => "https://api.x.ai/v1",
            Provider::OpenRouter => "https://openrouter.ai/api/v1",
            Provider::Synthetic => "https://api.synthetic.new/openai/v1",
        }
    }

    /// Переменная окружения, которая перебивает ключ из файлов.
    fn env_var(self) -> &'static str {
        match self {
            Provider::Yolo => "YOLO_API_KEY",
            Provider::Xai => "XAI_API_KEY",
            Provider::OpenRouter => "OPENROUTER_API_KEY",
            Provider::Synthetic => "SYNTHETIC_API_KEY",
        }
    }

    /// Где искать ключ, если переменной нет: файл + путь внутри JSON.
    /// Порядок значим — берётся первое найденное.
    fn key_sources(self) -> &'static [(&'static str, &'static [&'static str])] {
        match self {
            Provider::Yolo => &[(
                ".pi/agent/models.json",
                &["providers", "Yolo-Auto", "apiKey"],
            )],
            // OAuth-токен: в обоих файлах лежит один и тот же вход, но
            // протухают они независимо (см. `expired_oauth`).
            Provider::Xai => &[
                (".pi/agent/auth.json", &["xai", "access"]),
                (".local/share/opencode/auth.json", &["xai", "access"]),
            ],
            Provider::OpenRouter => &[
                (".pi/agent/auth.json", &["openrouter", "key"]),
                (".local/share/opencode/auth.json", &["openrouter", "key"]),
            ],
            Provider::Synthetic => &[(
                ".local/share/opencode/auth.json",
                &["synthetic", "key"],
            )],
        }
    }

    /// Правки тела запроса под конкретный API.
    fn adjust(self, body: &mut Value, effort: Option<Effort>) {
        match self {
            // Роутеру нужно явно попросить вернуть стоимость, иначе поля `cost`
            // в usage не будет — а это единственный источник «сколько списали
            // на самом деле» во всём приложении.
            Provider::OpenRouter => {
                body["usage"] = json!({ "include": true });
                if let Some(e) = effort {
                    body["reasoning"] = json!({ "effort": e.as_str() });
                }
            }
            // xAI принимает reasoning_effort как плоское поле.
            Provider::Xai => {
                if let Some(e) = effort {
                    body["reasoning_effort"] = json!(e.as_str());
                }
            }
            // Yolo и Synthetic отдают open-weights модели «как есть»: уровень
            // рассуждений у них не регулируется, лишнее поле только злит роутер.
            Provider::Yolo | Provider::Synthetic => {}
        }
    }
}

/// Уровень рассуждений. У каждой ступени лестницы он свой и зафиксирован
/// в `Tier` — это часть определения «средняя» и «сильная» модель.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        }
    }
}

/// Прайс модели, USD за миллион токенов.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Price {
    pub input: f64,
    pub output: f64,
}

impl Price {
    /// Цена конкретного ответа по прайсу. Токены рассуждений тарифицируются
    /// как выходные и уже входят в `completion_tokens` у всех четырёх площадок.
    pub fn cost(&self, prompt_tokens: u32, completion_tokens: u32) -> f64 {
        (prompt_tokens as f64 * self.input + completion_tokens as f64 * self.output) / 1e6
    }
}

/// Как оплачивается конкретный маршрут — это не то же самое, что прайс модели.
/// Одна и та же модель может стоить денег на своём API и ничего не стоить
/// по подписке; в отчёте нужны обе цифры, иначе «стоимость» врёт.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Billing {
    /// Платим за токены по прайсу.
    PerToken,
    /// Фиксированная подписка: токены сверху не списываются.
    Subscription,
}

#[derive(Clone)]
pub struct Client {
    pub provider: Provider,
    pub base_url: String,
    api_key: String,
}

impl Client {
    pub fn new(provider: Provider) -> Res<Client> {
        // Протухший OAuth-токен даёт невнятную 401 посреди прогона; лучше
        // сказать это до первого запроса и назвать, что чинить.
        if let Some(expired_at) = expired_oauth(provider) {
            return Err(format!(
                "{}: OAuth-токен истёк {} назад — войдите в аккаунт заново \
                 или задайте ${}",
                provider.id(),
                human_duration(expired_at),
                provider.env_var()
            ));
        }
        Ok(Client {
            base_url: env::var(format!("{}_BASE_URL", provider.env_var()))
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
            "temperature": req.temperature,
        });
        self.provider.adjust(&mut body, req.effort);

        let started = Instant::now();
        let resp = ureq::post(&format!("{}/chat/completions", self.base_url))
            .set("Authorization", &format!("Bearer {}", self.api_key))
            .set("Content-Type", "application/json")
            // Cloudflare перед yolo-auto отдаёт 403 (error 1010) на некоторые
            // клиентские User-Agent'ы — свой ставим явно, чтобы не зависеть
            // от того, чем представляется HTTP-библиотека.
            .set("User-Agent", "model-ladder/0.3")
            .timeout(Duration::from_secs(600))
            .send_json(body);
        let latency_ms = started.elapsed().as_millis() as u64;

        let text = match resp {
            Ok(r) => r.into_string().map_err(|e| format!("чтение ответа: {e}"))?,
            Err(ureq::Error::Status(code, r)) => {
                let detail = r.into_string().unwrap_or_default();
                return Err(format!(
                    "{} HTTP {code}: {}{}",
                    self.provider.id(),
                    truncate(&detail, 400),
                    // 401 на OAuth-площадке почти всегда значит «токен протух»,
                    // а не «ключ неверный» — подсказываем, что делать.
                    if code == 401 && self.provider == Provider::Xai {
                        " — похоже, OAuth-токен истёк, войдите в аккаунт заново"
                    } else {
                        ""
                    }
                ));
            }
            Err(e) => return Err(format!("сеть ({}): {e}", self.provider.id())),
        };

        let parsed: ApiResponse = serde_json::from_str(&text)
            .map_err(|e| format!("нераспознанный JSON ({e}): {}", truncate(&text, 300)))?;
        // OpenRouter умеет отдать ошибку с HTTP 200 — молча вернуть пустой
        // ответ хуже, чем упасть с текстом причины.
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
            // Какую модель площадка реально посчитала — не то же самое, что мы
            // просили. Расхождение ловит `verify.rs`.
            served_model: parsed.model.unwrap_or_default(),
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.billable_completion_tokens(),
            reasoning_tokens: usage.reasoning_tokens(),
            billed_usd: usage.cost,
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
    if let Ok(k) = env::var(provider.env_var()) {
        if !k.trim().is_empty() {
            return Ok(k);
        }
    }
    let mut tried = Vec::new();
    for (file, path) in provider.key_sources() {
        let full = home()?.join(file);
        tried.push(format!("{}:{}", file, path.join(".")));
        let Ok(raw) = fs::read_to_string(&full) else { continue };
        let Ok(json) = serde_json::from_str::<Value>(&raw) else { continue };
        if let Some(k) = dig(&json, path) {
            return Ok(k);
        }
    }
    Err(format!(
        "нет ключа для {}: пусто в ${} и не найдено в {}",
        provider.id(),
        provider.env_var(),
        tried.join(", ")
    ))
}

/// Значение по пути внутри JSON, если это непустая строка.
fn dig(v: &Value, path: &[&str]) -> Option<String> {
    let mut cur = v;
    for key in path {
        cur = cur.get(key)?;
    }
    cur.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

/// Сколько секунд назад истёк OAuth-токен площадки, если он истёк.
///
/// Смотрим только туда, откуда реально возьмём ключ: если ключ задан
/// переменной окружения, срок из файла к делу не относится.
fn expired_oauth(provider: Provider) -> Option<u64> {
    if provider != Provider::Xai || env::var(provider.env_var()).is_ok_and(|k| !k.trim().is_empty())
    {
        return None;
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;
    // Берём самый поздний срок из всех файлов: ключ мы возьмём из первого
    // подходящего, но входов может быть несколько, и живого достаточно одного.
    let latest = provider
        .key_sources()
        .iter()
        .filter_map(|(file, path)| {
            let raw = fs::read_to_string(home().ok()?.join(file)).ok()?;
            let json: Value = serde_json::from_str(&raw).ok()?;
            json.get(path[0])?.get("expires")?.as_u64()
        })
        .max()?;
    (latest < now_ms).then(|| (now_ms - latest) / 1000)
}

fn human_duration(secs: u64) -> String {
    match secs {
        s if s < 90 => format!("{s} с"),
        s if s < 5400 => format!("{} мин", s / 60),
        s if s < 172_800 => format!("{} ч", s / 3600),
        s => format!("{} дн", s / 86_400),
    }
}

fn home() -> Res<PathBuf> {
    env::var("HOME")
        .map(PathBuf::from)
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
    pub temperature: f64,
    pub max_tokens: u32,
    /// `None` — не просить конкретный уровень рассуждений (площадка не умеет).
    pub effort: Option<Effort>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reply {
    pub content: String,
    pub finish_reason: String,
    /// Модель, которую назвала сама площадка в ответе.
    pub served_model: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub reasoning_tokens: u32,
    /// Реально списанная сумма, если площадка её вернула (только OpenRouter).
    pub billed_usd: Option<f64>,
    pub latency_ms: u64,
}

#[derive(Deserialize)]
struct ApiResponse {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    model: Option<String>,
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
    /// OpenAI-каноничное место для токенов рассуждений.
    #[serde(default)]
    completion_tokens_details: Option<Details>,
    /// …а Yolo и Synthetic кладут то же самое плоским полем.
    #[serde(default)]
    reasoning_tokens: Option<u32>,
    #[serde(default)]
    total_tokens: u32,
    /// Только OpenRouter: сколько списано в USD.
    #[serde(default)]
    cost: Option<f64>,
}

impl Usage {
    /// Сколько выходных токенов реально оплачивается и реально ждётся.
    ///
    /// Площадки расходятся: у Yolo, Synthetic и OpenRouter токены рассуждений
    /// уже входят в `completion_tokens`, а xAI выносит их наружу — там
    /// `completion_tokens` это только видимый текст, и `prompt + completion`
    /// не сходится с `total_tokens`. Верить в этом случае надо `total_tokens`:
    /// иначе рассуждающая модель выглядит и втрое дешевле, и втрое медленнее,
    /// чем есть, потому что самая дорогая часть работы просто не посчитана.
    fn billable_completion_tokens(&self) -> u32 {
        let declared = self.total_tokens.saturating_sub(self.prompt_tokens);
        declared.max(self.completion_tokens)
    }

    fn reasoning_tokens(&self) -> u32 {
        self.completion_tokens_details
            .as_ref()
            .map(|d| d.reasoning_tokens)
            .filter(|&t| t > 0)
            .or(self.reasoning_tokens)
            .unwrap_or(0)
    }
}

#[derive(Deserialize)]
struct Details {
    #[serde(default)]
    reasoning_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> Request {
        Request {
            model: "m".into(),
            system: Some("sys".into()),
            prompt: "hi".into(),
            temperature: 0.2,
            max_tokens: 10,
            effort: Some(Effort::High),
        }
    }

    #[test]
    fn system_message_goes_first() {
        let m = messages_of(&req());
        assert_eq!(m[0]["role"], "system");
        assert_eq!(m[1]["role"], "user");
        assert_eq!(m[1]["content"], "hi");
    }

    #[test]
    fn no_system_means_single_user_message() {
        let mut r = req();
        r.system = None;
        let m = messages_of(&r);
        assert_eq!(m.as_array().unwrap().len(), 1);
        assert_eq!(m[0]["role"], "user");
    }

    /// Без `usage.include` роутер не вернёт `cost`, и колонка «списано»
    /// в отчёте окажется пустой на единственной площадке, которая её знает.
    #[test]
    fn openrouter_always_asks_for_the_cost_field() {
        let mut body = json!({});
        Provider::OpenRouter.adjust(&mut body, Some(Effort::High));
        assert_eq!(body["usage"]["include"], true);
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    #[test]
    fn xai_takes_effort_as_a_flat_field() {
        let mut body = json!({});
        Provider::Xai.adjust(&mut body, Some(Effort::Medium));
        assert_eq!(body["reasoning_effort"], "medium");
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn open_weights_routes_get_no_effort_knob() {
        for p in [Provider::Yolo, Provider::Synthetic] {
            let mut body = json!({});
            p.adjust(&mut body, Some(Effort::High));
            assert_eq!(body, json!({}));
        }
    }

    #[test]
    fn effort_is_omitted_when_not_requested() {
        let mut body = json!({});
        Provider::Xai.adjust(&mut body, None);
        assert!(body.get("reasoning_effort").is_none());
    }

    /// Три из четырёх площадок кладут токены рассуждений плоским полем, а не
    /// в `completion_tokens_details`. Потерять их — занизить ресурсоёмкость.
    #[test]
    fn reasoning_tokens_are_read_from_either_shape() {
        let nested: Usage =
            serde_json::from_value(json!({ "completion_tokens_details": { "reasoning_tokens": 7 } }))
                .unwrap();
        assert_eq!(nested.reasoning_tokens(), 7);

        let flat: Usage = serde_json::from_value(json!({ "reasoning_tokens": 13 })).unwrap();
        assert_eq!(flat.reasoning_tokens(), 13);

        // OpenRouter присылает обе формы, но вложенную с нулём.
        let both: Usage = serde_json::from_value(json!({
            "reasoning_tokens": 13,
            "completion_tokens_details": { "reasoning_tokens": 0 }
        }))
        .unwrap();
        assert_eq!(both.reasoning_tokens(), 13);

        let none: Usage = serde_json::from_value(json!({})).unwrap();
        assert_eq!(none.reasoning_tokens(), 0);
    }

    /// Настоящие тела ответов четырёх площадок: три сходятся по
    /// `prompt + completion == total`, а xAI — нет, и именно там нельзя
    /// брать `completion_tokens` как есть.
    #[test]
    fn xai_reasoning_tokens_are_added_back_to_the_billable_output() {
        // xAI: 646 prompt + 2 видимых + 202 рассуждений = 850.
        let xai: Usage = serde_json::from_value(json!({
            "prompt_tokens": 646, "completion_tokens": 2, "total_tokens": 850,
            "completion_tokens_details": { "reasoning_tokens": 202 }
        }))
        .unwrap();
        assert_eq!(xai.billable_completion_tokens(), 204);

        // Остальные уже всё сложили — трогать их нельзя.
        for u in [
            json!({ "prompt_tokens": 19, "completion_tokens": 33, "total_tokens": 52 }),
            json!({ "prompt_tokens": 21, "completion_tokens": 5, "total_tokens": 26 }),
            json!({ "prompt_tokens": 101, "completion_tokens": 26, "total_tokens": 127 }),
        ] {
            let expected = u["completion_tokens"].as_u64().unwrap() as u32;
            let usage: Usage = serde_json::from_value(u).unwrap();
            assert_eq!(usage.billable_completion_tokens(), expected);
        }
    }

    /// Площадка может не прислать `total_tokens` вовсе — тогда единственное,
    /// что у нас есть, это `completion_tokens`, и занижать его нельзя.
    #[test]
    fn a_missing_total_falls_back_to_the_completion_count() {
        let u: Usage =
            serde_json::from_value(json!({ "prompt_tokens": 100, "completion_tokens": 40 }))
                .unwrap();
        assert_eq!(u.billable_completion_tokens(), 40);
    }

    #[test]
    fn price_counts_both_directions_per_million() {
        let p = Price { input: 5.0, output: 25.0 };
        assert!((p.cost(1_000_000, 0) - 5.0).abs() < 1e-12);
        assert!((p.cost(0, 1_000_000) - 25.0).abs() < 1e-12);
        assert!((p.cost(200, 100) - (200.0 * 5.0 + 100.0 * 25.0) / 1e6).abs() < 1e-12);
    }

    #[test]
    fn dig_walks_the_path_and_rejects_empty() {
        let v = json!({ "a": { "b": "x" }, "empty": { "b": "" } });
        assert_eq!(dig(&v, &["a", "b"]), Some("x".into()));
        assert_eq!(dig(&v, &["empty", "b"]), None);
        assert_eq!(dig(&v, &["a", "nope"]), None);
    }

    /// Каждая площадка обязана знать, где лежит её ключ: пустой список
    /// источников — это молчаливая невозможность запустить ступень.
    #[test]
    fn every_provider_knows_where_its_key_lives() {
        for p in [
            Provider::Yolo,
            Provider::Xai,
            Provider::OpenRouter,
            Provider::Synthetic,
        ] {
            assert!(!p.key_sources().is_empty(), "{}", p.id());
            assert!(p.env_var().ends_with("_API_KEY"), "{}", p.id());
            assert!(p.default_base_url().ends_with("/v1"), "{}", p.id());
        }
    }

    /// Срок жизни есть только у OAuth-площадки; ключам-строкам он не грозит.
    #[test]
    fn only_the_oauth_route_can_expire() {
        for p in [Provider::Yolo, Provider::OpenRouter, Provider::Synthetic] {
            assert_eq!(expired_oauth(p), None, "{}", p.id());
        }
    }

    #[test]
    fn human_duration_scales_the_unit() {
        assert_eq!(human_duration(30), "30 с");
        assert_eq!(human_duration(600), "10 мин");
        assert_eq!(human_duration(7200), "2 ч");
        assert_eq!(human_duration(300_000), "3 дн");
    }

    #[test]
    fn truncate_counts_chars_not_bytes() {
        assert_eq!(truncate("привет", 3), "при…");
        assert_eq!(truncate("привет", 6), "привет");
    }
}
