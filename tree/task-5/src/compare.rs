//! Один и тот же запрос при temperature 0 / 0.7 / 1.2, замер расхождений,
//! затем разбор старшей моделью.
//!
//! Важное для честности эксперимента:
//! * все три ветки получают **побайтово одинаковые** system+user сообщения;
//! * меняется ровно одно поле — `temperature`;
//! * «разнообразие» не берётся со слов модели-судьи, а считается здесь:
//!   несколько прогонов на одну температуру + попарная мера Жаккара.

use serde::{Deserialize, Serialize};
use std::thread;

use crate::api::{Client, Reply, Request, Res, JUDGE_MODEL, JUDGE_PROVIDER};

/// Три точки из задания.
pub const TEMPERATURES: [f64; 3] = [0.0, 0.7, 1.2];

const SYSTEM: &str = "Ты отвечаешь по-русски, по существу и без вступлений вроде «Конечно!».";
const MAX_TOKENS: u32 = 900;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Run {
    pub content: String,
    pub finish_reason: String,
    pub completion_tokens: u32,
    pub reasoning_tokens: u32,
    pub latency_ms: u64,
}

impl From<Reply> for Run {
    fn from(r: Reply) -> Run {
        Run {
            content: r.content,
            finish_reason: r.finish_reason,
            completion_tokens: r.completion_tokens,
            reasoning_tokens: r.reasoning_tokens,
            latency_ms: r.latency_ms,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Branch {
    pub temperature: f64,
    /// Ответ, который показываем как «тот самый» для этой температуры.
    pub answer: String,
    /// Все прогоны (первый совпадает с `answer`).
    pub runs: Vec<Run>,
    pub metrics: Metrics,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Metrics {
    pub chars: usize,
    pub words: usize,
    /// Доля уникальных слов — лексическое богатство одного ответа.
    pub distinct_ratio: f64,
    /// Среднее попарное сходство прогонов одной температуры (Жаккар, 0..1).
    /// `None`, если прогон был один. Меньше — разнообразнее.
    pub self_similarity: Option<f64>,
    pub avg_completion_tokens: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Comparison {
    pub prompt: String,
    pub worker_provider: String,
    pub worker_model: String,
    pub judge_model: String,
    pub runs_per_temperature: usize,
    pub branches: Vec<Branch>,
    /// Разбор от старшей модели (markdown).
    pub summary: String,
    /// Есть ли смысл верить различиям: результат `verify::check_temperature`.
    /// `None` — проверку не запускали.
    pub temp_check: Option<crate::verify::TempCheck>,
    pub total_ms: u64,
}

/// Три ветки идут параллельно (внутри ветки прогоны подряд, чтобы не долбить
/// API девятью одновременными запросами), затем разбор старшей моделью.
///
/// `answers` — провайдер, которому задаём температуры; судья всегда свой
/// (`JUDGE_PROVIDER`), поэтому подключается отдельным клиентом.
pub fn run(answers: &Client, prompt: &str, runs_per_temperature: usize) -> Res<Comparison> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Err("пустой запрос".into());
    }
    let n = runs_per_temperature.clamp(1, 5);
    let started = std::time::Instant::now();

    let branches: Vec<Branch> = thread::scope(|scope| {
        let handles: Vec<_> = TEMPERATURES
            .iter()
            .map(|&t| scope.spawn(move || branch(answers, prompt, t, n)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().map_err(|_| "поток ветки упал".to_string())?)
            .collect::<Res<Vec<Branch>>>()
    })?;

    let judge_client = Client::new(JUDGE_PROVIDER)?;
    let summary = judge(&judge_client, prompt, &branches)?;

    Ok(Comparison {
        prompt: prompt.to_string(),
        worker_provider: answers.provider.id().into(),
        worker_model: answers.provider.answer_model().into(),
        judge_model: JUDGE_MODEL.into(),
        runs_per_temperature: n,
        branches,
        summary,
        temp_check: None,
        total_ms: started.elapsed().as_millis() as u64,
    })
}

fn branch(client: &Client, prompt: &str, temperature: f64, n: usize) -> Res<Branch> {
    let req = Request {
        model: client.provider.answer_model().into(),
        system: Some(SYSTEM.into()),
        prompt: prompt.to_string(),
        temperature: Some(temperature),
        max_tokens: MAX_TOKENS,
        thinking: false,
    };
    let mut runs = Vec::with_capacity(n);
    for i in 0..n {
        let reply = client
            .complete(&req)
            .map_err(|e| format!("temperature={temperature}, прогон {}: {e}", i + 1))?;
        runs.push(Run::from(reply));
    }
    let metrics = metrics_of(&runs);
    Ok(Branch {
        temperature,
        answer: runs[0].content.clone(),
        runs,
        metrics,
    })
}

fn metrics_of(runs: &[Run]) -> Metrics {
    let first = &runs[0].content;
    let words = tokenize(first);
    let unique: std::collections::BTreeSet<&String> = words.iter().collect();
    let sets: Vec<std::collections::BTreeSet<String>> = runs
        .iter()
        .map(|r| tokenize(&r.content).into_iter().collect())
        .collect();

    let mut pairs = Vec::new();
    for i in 0..sets.len() {
        for j in (i + 1)..sets.len() {
            pairs.push(jaccard(&sets[i], &sets[j]));
        }
    }

    Metrics {
        chars: first.chars().count(),
        words: words.len(),
        distinct_ratio: if words.is_empty() {
            0.0
        } else {
            unique.len() as f64 / words.len() as f64
        },
        self_similarity: if pairs.is_empty() {
            None
        } else {
            Some(pairs.iter().sum::<f64>() / pairs.len() as f64)
        },
        avg_completion_tokens: runs.iter().map(|r| r.completion_tokens as f64).sum::<f64>()
            / runs.len() as f64,
    }
}

/// Слова в нижнем регистре без пунктуации — основа и для лексики, и для Жаккара.
fn tokenize(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .collect()
}

fn jaccard(a: &std::collections::BTreeSet<String>, b: &std::collections::BTreeSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}

fn judge(client: &Client, prompt: &str, branches: &[Branch]) -> Res<String> {
    let mut brief = String::new();
    for b in branches {
        brief.push_str(&format!(
            "\n### temperature = {}\n(слов: {}, уникальных слов: {:.0}%{})\n{}\n",
            b.temperature,
            b.metrics.words,
            b.metrics.distinct_ratio * 100.0,
            match b.metrics.self_similarity {
                Some(s) => format!(", совпадение повторных прогонов: {:.0}%", s * 100.0),
                None => String::new(),
            },
            b.answer
        ));
    }

    let req = Request {
        model: JUDGE_MODEL.into(),
        system: Some(
            "Ты — методист по работе с LLM. Пишешь по-русски, коротко, без воды и без похвал."
                .into(),
        ),
        prompt: format!(
            "Одна и та же задача была отправлена одной и той же модели три раза; \
менялся ровно один параметр — temperature. Разбери результат.\n\n\
ИСХОДНАЯ ЗАДАЧА:\n{prompt}\n\nОТВЕТЫ:\n{brief}\n\n\
Ответь строго в этом формате markdown, без вступления:\n\n\
## Точность\nОдин абзац: где ответ ближе к фактам/инструкции и почему.\n\n\
## Креативность\nОдин абзац с конкретными примерами формулировок из ответов.\n\n\
## Разнообразие\nОдин абзац; опирайся на цифры совпадения повторных прогонов, если они есть.\n\n\
## Когда какую температуру брать\n- **0** — ...\n- **0.7** — ...\n- **1.2** — ...\n\n\
## Вывод\nОдно-два предложения.\n\n\
Если разница между ответами мала — так и напиши, не выдумывай различий."
        ),
        temperature: Some(0.2),
        max_tokens: 2000,
        thinking: true,
    };
    Ok(client.complete(&req)?.content)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_of(text: &str) -> Run {
        Run {
            content: text.into(),
            finish_reason: "stop".into(),
            completion_tokens: 10,
            reasoning_tokens: 0,
            latency_ms: 1,
        }
    }

    #[test]
    fn identical_runs_have_full_self_similarity() {
        let m = metrics_of(&[run_of("море и ветер"), run_of("море и ветер")]);
        assert_eq!(m.self_similarity, Some(1.0));
    }

    #[test]
    fn disjoint_runs_have_zero_self_similarity() {
        let m = metrics_of(&[run_of("море ветер"), run_of("камень песок")]);
        assert_eq!(m.self_similarity, Some(0.0));
    }

    #[test]
    fn single_run_reports_no_self_similarity() {
        let m = metrics_of(&[run_of("море")]);
        assert!(m.self_similarity.is_none());
    }

    #[test]
    fn distinct_ratio_counts_repeats() {
        let m = metrics_of(&[run_of("море море ветер")]);
        assert_eq!(m.words, 3);
        assert!((m.distinct_ratio - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn tokenize_drops_punctuation_and_case() {
        assert_eq!(tokenize("Море, Ветер!"), vec!["море", "ветер"]);
    }

    #[test]
    fn jaccard_of_partial_overlap() {
        let a: std::collections::BTreeSet<String> = tokenize("a b c").into_iter().collect();
        let b: std::collections::BTreeSet<String> = tokenize("b c d").into_iter().collect();
        assert!((jaccard(&a, &b) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn empty_prompt_is_rejected_before_any_request() {
        assert!(run(&Client::dummy(), "   ", 1).is_err());
    }
}
