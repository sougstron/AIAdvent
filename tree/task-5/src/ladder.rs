//! Один и тот же запрос на трёх ступенях — слабая / средняя / сильная модель,
//! замер времени, токенов и стоимости, затем разбор четвёртой моделью-судьёй.
//!
//! Что здесь сделано ради честности сравнения:
//! * все три ступени получают **побайтово одинаковые** system+user сообщения,
//!   один и тот же `max_tokens` и одну и ту же `temperature`;
//! * ступени идут параллельно, но время меряется по каждому запросу отдельно,
//!   поэтому параллельность не искажает замер латентности;
//! * судья получает ответы **вслепую** — без имён моделей и без цифр, только
//!   тексты под номерами, иначе он оценивает репутацию, а не ответ;
//! * стоимость считается из прайса по токенам, а не берётся со слов модели.

use serde::{Deserialize, Serialize};
use std::thread;

use crate::api::{Billing, Client, Effort, Price, Provider, Reply, Request, Res};

/// Общая для всех ступеней рамка запроса. Меняется ровно одно — модель.
const SYSTEM: &str = "Ты отвечаешь по-русски, по существу и без вступлений вроде «Конечно!».";
pub const TEMPERATURE: f64 = 0.3;
pub const MAX_TOKENS: u32 = 1200;

/// Ступень лестницы: всё, что отличает «слабую» модель от «сильной», включая
/// цену и ссылки — их требует итоговый отчёт.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Weak,
    Medium,
    Strong,
}

/// Паспорт ступени. Прайс — публичная цена модели, `billing` — как оплачен
/// конкретно этот маршрут; это разные вещи, и в отчёте они разведены.
pub struct Spec {
    pub tier: Tier,
    pub label: &'static str,
    pub provider: Provider,
    pub model: &'static str,
    pub effort: Option<Effort>,
    pub price: Price,
    pub billing: Billing,
    /// Откуда взят прайс и где живёт сама модель — в отчёт «+ ссылки».
    pub model_url: &'static str,
    pub price_url: &'static str,
}

/// Три ступени задания. Слабая — открытая 27B через реселлера, средняя —
/// рассуждающая модель среднего размера, сильная — фронтир на high.
pub const LADDER: [Spec; 3] = [
    Spec {
        tier: Tier::Weak,
        label: "слабая",
        provider: Provider::Yolo,
        model: "qwen3.8-27b",
        // Маршрут отдаёт открытые веса как есть, уровень рассуждений не
        // регулируется — модель думает столько, сколько считает нужным.
        effort: None,
        // Прайс той же модели у платного хостера — маршрут через yolo для нас
        // бесплатный, но «бесплатно» и «ничего не стоит» это разные утверждения.
        price: Price { input: 0.42, output: 3.00 },
        billing: Billing::Subscription,
        model_url: "https://huggingface.co/Qwen/Qwen3.8-27B",
        price_url: "https://openrouter.ai/qwen/qwen3.8-27b",
    },
    Spec {
        tier: Tier::Medium,
        label: "средняя",
        provider: Provider::Xai,
        model: "grok-4.6",
        effort: Some(Effort::Medium),
        price: Price { input: 2.00, output: 6.00 },
        billing: Billing::PerToken,
        model_url: "https://docs.x.ai/docs/models",
        price_url: "https://x.ai/api",
    },
    Spec {
        tier: Tier::Strong,
        label: "сильная",
        provider: Provider::OpenRouter,
        model: "anthropic/claude-opus-5",
        effort: Some(Effort::High),
        price: Price { input: 5.00, output: 25.00 },
        billing: Billing::PerToken,
        model_url: "https://openrouter.ai/anthropic/claude-opus-5",
        price_url: "https://www.anthropic.com/pricing",
    },
];

/// Судья — четвёртая модель, намеренно не из лестницы: пусть оценивает
/// со стороны, а не сравнивает себя с собой.
pub const JUDGE_PROVIDER: Provider = Provider::Synthetic;
pub const JUDGE_MODEL: &str = "hf:moonshotai/Kimi-K3";
pub const JUDGE_URL: &str = "https://huggingface.co/moonshotai/Kimi-K3";

/// Замеры одной ступени. Всё, что просит задание — время, токены, стоимость —
/// плюс производные, по которым удобно сравнивать ресурсоёмкость.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Measure {
    pub latency_ms: u64,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// Токены рассуждений — часть `completion_tokens`, но платим и ждём мы
    /// за них так же, как за видимый текст.
    pub reasoning_tokens: u32,
    /// Выходных токенов в секунду — «скорость» в чистом виде.
    pub tokens_per_sec: f64,
    /// Цена запроса по прайсу модели, USD.
    pub cost_usd: f64,
    /// Сколько списала площадка, если она это сообщает (только OpenRouter).
    pub billed_usd: Option<f64>,
    pub chars: usize,
    pub words: usize,
    /// Доля уникальных слов — грубая мера, не «вода» ли ответ.
    pub distinct_ratio: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rung {
    pub tier: Tier,
    pub label: String,
    pub provider: String,
    pub model: String,
    /// Модель, которую назвала сама площадка (может отличаться от `model`).
    pub served_model: String,
    pub effort: Option<Effort>,
    pub billing: Billing,
    pub model_url: String,
    pub price_url: String,
    pub answer: String,
    pub finish_reason: String,
    pub measure: Measure,
    /// Оценка судьи 1..10, если её удалось разобрать из ответа.
    pub score: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ladder {
    pub prompt: String,
    pub judge_model: String,
    pub judge_url: String,
    pub temperature: f64,
    pub max_tokens: u32,
    pub rungs: Vec<Rung>,
    /// Разбор судьи (markdown).
    pub summary: String,
    /// Проверка, что лестница — лестница, а не три ярлыка.
    pub check: Option<crate::verify::LadderCheck>,
    pub total_ms: u64,
}

/// Три ступени идут параллельно: суммарное время ожидания — это максимум,
/// а не сумма, и на замер латентности каждой ступени это не влияет.
pub fn run(prompt: &str) -> Res<Ladder> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Err("пустой запрос".into());
    }
    let started = std::time::Instant::now();

    let mut rungs: Vec<Rung> = thread::scope(|scope| {
        let handles: Vec<_> = LADDER
            .iter()
            .map(|s| scope.spawn(move || rung(s, prompt)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().map_err(|_| "поток ступени упал".to_string())?)
            .collect::<Res<Vec<Rung>>>()
    })?;

    let judge_client = Client::new(JUDGE_PROVIDER)?;
    let summary = judge(&judge_client, prompt, &rungs)?;
    for (i, r) in rungs.iter_mut().enumerate() {
        r.score = parse_score(&summary, i + 1);
    }

    Ok(Ladder {
        prompt: prompt.to_string(),
        judge_model: JUDGE_MODEL.into(),
        judge_url: JUDGE_URL.into(),
        temperature: TEMPERATURE,
        max_tokens: MAX_TOKENS,
        rungs,
        summary,
        check: None,
        total_ms: started.elapsed().as_millis() as u64,
    })
}

fn rung(s: &Spec, prompt: &str) -> Res<Rung> {
    let client = Client::new(s.provider)?;
    let reply = client
        .complete(&Request {
            model: s.model.into(),
            system: Some(SYSTEM.into()),
            prompt: prompt.to_string(),
            temperature: TEMPERATURE,
            max_tokens: MAX_TOKENS,
            effort: s.effort,
        })
        .map_err(|e| format!("{} ({}): {e}", s.label, s.model))?;

    Ok(Rung {
        tier: s.tier,
        label: s.label.into(),
        provider: s.provider.id().into(),
        model: s.model.into(),
        served_model: reply.served_model.clone(),
        effort: s.effort,
        billing: s.billing,
        model_url: s.model_url.into(),
        price_url: s.price_url.into(),
        finish_reason: reply.finish_reason.clone(),
        measure: measure_of(s, &reply),
        answer: reply.content,
        score: None,
    })
}

fn measure_of(s: &Spec, r: &Reply) -> Measure {
    let words = tokenize(&r.content);
    let unique: std::collections::BTreeSet<&String> = words.iter().collect();
    Measure {
        latency_ms: r.latency_ms,
        prompt_tokens: r.prompt_tokens,
        completion_tokens: r.completion_tokens,
        reasoning_tokens: r.reasoning_tokens,
        tokens_per_sec: if r.latency_ms == 0 {
            0.0
        } else {
            r.completion_tokens as f64 * 1000.0 / r.latency_ms as f64
        },
        cost_usd: s.price.cost(r.prompt_tokens, r.completion_tokens),
        billed_usd: r.billed_usd,
        chars: r.content.chars().count(),
        words: words.len(),
        distinct_ratio: if words.is_empty() {
            0.0
        } else {
            unique.len() as f64 / words.len() as f64
        },
    }
}

fn tokenize(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .collect()
}

/// Судья не должен знать, где чей ответ: имена моделей и цифры замеров
/// в его промпт не попадают вообще.
fn judge(client: &Client, prompt: &str, rungs: &[Rung]) -> Res<String> {
    let mut brief = String::new();
    for (i, r) in rungs.iter().enumerate() {
        brief.push_str(&format!("\n### Ответ {}\n{}\n", i + 1, r.answer));
    }

    let req = Request {
        model: JUDGE_MODEL.into(),
        system: Some(
            "Ты — независимый эксперт-оценщик. Пишешь по-русски, коротко, без воды и без похвал."
                .into(),
        ),
        prompt: format!(
            "Один и тот же запрос был отправлен трём разным языковым моделям. \
Кто есть кто — тебе не сообщают намеренно: оценивай только тексты.\n\n\
ЗАПРОС:\n{prompt}\n\nОТВЕТЫ:\n{brief}\n\n\
Ответь строго в этом формате markdown, без вступления:\n\n\
ОЦЕНКИ: 1=X, 2=X, 3=X\n\
(целые от 1 до 10 — общее качество ответа: фактическая верность, \
полнота, структура, отсутствие воды)\n\n\
## Что различает ответы\nОдин абзац с конкретными примерами формулировок.\n\n\
## Ошибки и слабые места\nСписком, с указанием номера ответа. \
Если ошибок нет — так и напиши.\n\n\
## Вывод\nОдно-два предложения: какой ответ лучше и стоит ли разница усилий.\n\n\
Если разница между ответами мала — так и напиши, не выдумывай различий."
        ),
        temperature: 0.2,
        max_tokens: 2000,
        effort: None,
    };
    Ok(client.complete(&req)?.content)
}

/// Достаёт `n=X` из строки «ОЦЕНКИ: 1=8, 2=9, 3=9».
///
/// Отдельная функция, потому что это единственное место, где мы верим тексту
/// модели: не разобралось — в отчёте будет прочерк, а не выдуманное число.
fn parse_score(summary: &str, n: usize) -> Option<f64> {
    let line = summary
        .lines()
        .find(|l| l.trim_start().to_uppercase().starts_with("ОЦЕНКИ"))?;
    let needle = format!("{n}=");
    let rest = line.split(&needle).nth(1)?;
    let num: String = rest
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    num.parse::<f64>().ok().filter(|v| (1.0..=10.0).contains(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder_goes_weak_medium_strong_and_costs_more_each_step() {
        assert_eq!(LADDER.len(), 3);
        assert_eq!(LADDER[0].tier, Tier::Weak);
        assert_eq!(LADDER[2].tier, Tier::Strong);
        for w in LADDER.windows(2) {
            assert!(
                w[1].price.output > w[0].price.output,
                "{} должна быть дороже {}",
                w[1].model,
                w[0].model
            );
        }
    }

    /// Судья обязан быть вне лестницы, иначе он оценивает в том числе себя.
    #[test]
    fn judge_is_not_one_of_the_rungs() {
        assert!(LADDER.iter().all(|s| s.provider != JUDGE_PROVIDER));
        assert!(LADDER.iter().all(|s| s.model != JUDGE_MODEL));
    }

    #[test]
    fn every_rung_carries_links_for_the_report() {
        for s in &LADDER {
            assert!(s.model_url.starts_with("https://"), "{}", s.model);
            assert!(s.price_url.starts_with("https://"), "{}", s.model);
        }
    }

    #[test]
    fn scores_are_parsed_from_the_judge_line() {
        let s = "ОЦЕНКИ: 1=6, 2=8, 3=9\n\n## Что различает";
        assert_eq!(parse_score(s, 1), Some(6.0));
        assert_eq!(parse_score(s, 2), Some(8.0));
        assert_eq!(parse_score(s, 3), Some(9.0));
    }

    #[test]
    fn a_ten_is_not_truncated_to_one() {
        assert_eq!(parse_score("ОЦЕНКИ: 1=10, 2=1, 3=7", 1), Some(10.0));
    }

    /// Лучше прочерк в отчёте, чем выдуманное число: всё, что не разобралось
    /// или вышло за шкалу, обязано вернуть None.
    #[test]
    fn unparsable_or_out_of_range_scores_are_dropped() {
        assert_eq!(parse_score("ОЦЕНКИ: 1=высокая", 1), None);
        assert_eq!(parse_score("ОЦЕНКИ: 1=42", 1), None);
        assert_eq!(parse_score("ОЦЕНКИ: 1=0", 1), None);
        assert_eq!(parse_score("судья решил не ставить оценок", 1), None);
        assert_eq!(parse_score("ОЦЕНКИ: 1=8", 3), None);
    }

    #[test]
    fn empty_prompt_is_rejected_before_any_request() {
        assert!(run("   ").is_err());
    }

    #[test]
    fn tokens_per_sec_survives_a_zero_latency_reading() {
        let s = &LADDER[0];
        let m = measure_of(
            s,
            &Reply {
                content: "раз два раз".into(),
                finish_reason: "stop".into(),
                served_model: s.model.into(),
                prompt_tokens: 10,
                completion_tokens: 20,
                reasoning_tokens: 0,
                billed_usd: None,
                latency_ms: 0,
            },
        );
        assert_eq!(m.tokens_per_sec, 0.0);
        assert_eq!(m.words, 3);
        assert!((m.distinct_ratio - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn cost_uses_the_price_of_that_rung() {
        let s = &LADDER[2];
        let m = measure_of(
            s,
            &Reply {
                content: "ответ".into(),
                finish_reason: "stop".into(),
                served_model: s.model.into(),
                prompt_tokens: 1000,
                completion_tokens: 1000,
                reasoning_tokens: 400,
                billed_usd: Some(0.031),
                latency_ms: 2000,
            },
        );
        assert!((m.cost_usd - (1000.0 * 5.0 + 1000.0 * 25.0) / 1e6).abs() < 1e-12);
        assert_eq!(m.billed_usd, Some(0.031));
        assert!((m.tokens_per_sec - 500.0).abs() < 1e-9);
    }
}
