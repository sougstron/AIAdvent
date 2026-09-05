//! Выгрузка сравнения в markdown — это и есть сдаваемый артефакт задания
//! («примеры ответов с разной температурой и выводы по их использованию»).

use crate::compare::Comparison;
use crate::verify::{TempCheck, Verdict};

pub fn to_markdown(c: &Comparison) -> String {
    let mut out = String::new();
    out.push_str("# Температура: один запрос, три настройки\n\n");
    out.push_str(&format!(
        "- **Запрос:** {}\n- **Модель ответов:** `{}` ({})\n- **Модель разбора:** `{}`\n\
         - **Прогонов на температуру:** {}\n- **Общее время:** {:.1} с\n\n",
        c.prompt,
        c.worker_model,
        c.worker_provider,
        c.judge_model,
        c.runs_per_temperature,
        c.total_ms as f64 / 1000.0
    ));

    if let Some(check) = &c.temp_check {
        out.push_str(&check_section(check));
    }

    out.push_str("## Сводка по метрикам\n\n");
    out.push_str("| temperature | слов | уникальных слов | совпадение прогонов | токенов ответа |\n");
    out.push_str("|---|---|---|---|---|\n");
    for b in &c.branches {
        out.push_str(&format!(
            "| {} | {} | {:.0}% | {} | {:.0} |\n",
            b.temperature,
            b.metrics.words,
            b.metrics.distinct_ratio * 100.0,
            match b.metrics.self_similarity {
                Some(s) => format!("{:.0}%", s * 100.0),
                None => "—".into(),
            },
            b.metrics.avg_completion_tokens
        ));
    }
    out.push_str(
        "\n«Совпадение прогонов» — средняя мера Жаккара между повторами одного и того же \
         запроса при этой температуре. Чем ниже, тем разнообразнее выдача.\n\n",
    );

    out.push_str("## Ответы\n");
    for b in &c.branches {
        out.push_str(&format!("\n### temperature = {}\n\n", b.temperature));
        for (i, r) in b.runs.iter().enumerate() {
            if b.runs.len() > 1 {
                out.push_str(&format!("**Прогон {}** ", i + 1));
            }
            out.push_str(&format!(
                "<sub>{} ток., {} мс, finish_reason: `{}`</sub>\n\n",
                r.completion_tokens, r.latency_ms, r.finish_reason
            ));
            out.push_str(&format!("> {}\n\n", r.content.replace('\n', "\n> ")));
        }
    }

    out.push_str(&format!("## Разбор ({})\n\n{}\n", c.judge_model, c.summary));
    out
}

/// Ставится в начало отчёта: без неё непонятно, можно ли вообще верить
/// различиям между ветками.
fn check_section(c: &TempCheck) -> String {
    let (label, note) = match c.verdict {
        Verdict::Honored => (
            "✅ параметр применяется",
            "Различия ниже вызваны именно температурой.",
        ),
        Verdict::Ignored => (
            "❌ параметр игнорируется",
            "**Различия ниже к температуре отношения не имеют** — это обычный разброс сэмплинга.",
        ),
        Verdict::Inconclusive => (
            "⚠️ проверка не показательна",
            "Проба не доказывает ни работу параметра, ни его игнорирование.",
        ),
    };
    format!(
        "## Проверка: доезжает ли `temperature` до сэмплера\n\n\
         **{label}** — `{}` / `{}`\n\n\
         | режим | ответы пробы | различных |\n|---|---|---|\n\
         | `temperature = {}` | {} | {} |\n| `temperature = {}` | {} | {} |\n\n\
         {}\n\n{note}\n\n",
        c.provider,
        c.model,
        c.cold_temperature,
        probe_cell(&c.cold_answers),
        c.cold_distinct,
        c.hot_temperature,
        probe_cell(&c.hot_answers),
        c.hot_distinct,
        c.explanation
    )
}

/// Ответы пробы — сырой вывод модели: в нём попадаются и `**жирный**`, и `|`,
/// которые иначе разъезжают таблицу. Экранируем кодом.
fn probe_cell(answers: &[String]) -> String {
    answers
        .iter()
        .map(|a| format!("`{}`", a.replace('`', "'").replace('|', "\\|")))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::{Branch, Metrics, Run};

    fn comparison() -> Comparison {
        Comparison {
            prompt: "тест".into(),
            worker_provider: "p".into(),
            worker_model: "w".into(),
            judge_model: "j".into(),
            runs_per_temperature: 1,
            branches: vec![Branch {
                temperature: 0.7,
                answer: "первая строка\nвторая строка".into(),
                runs: vec![Run {
                    content: "первая строка\nвторая строка".into(),
                    finish_reason: "stop".into(),
                    completion_tokens: 5,
                    reasoning_tokens: 0,
                    latency_ms: 12,
                }],
                metrics: Metrics {
                    chars: 27,
                    words: 4,
                    distinct_ratio: 0.5,
                    self_similarity: None,
                    avg_completion_tokens: 5.0,
                },
            }],
            summary: "вывод".into(),
            temp_check: None,
            total_ms: 1500,
        }
    }

    #[test]
    fn multiline_answers_stay_inside_the_blockquote() {
        let md = to_markdown(&comparison());
        assert!(md.contains("> первая строка\n> вторая строка"));
    }

    #[test]
    fn missing_self_similarity_renders_as_dash() {
        assert!(to_markdown(&comparison()).contains("| 0.7 | 4 | 50% | — | 5 |"));
    }

    #[test]
    fn no_check_means_no_verification_section() {
        assert!(!to_markdown(&comparison()).contains("доезжает ли"));
    }

    /// Самое важное свойство отчёта: если провайдер игнорирует параметр,
    /// читатель обязан узнать это раньше, чем увидит «различия».
    #[test]
    fn ignored_verdict_warns_before_the_answers() {
        let mut c = comparison();
        c.temp_check = Some(TempCheck {
            provider: "zai".into(),
            model: "glm-5.3-flash".into(),
            samples: 2,
            cold_temperature: 0.0,
            hot_temperature: 2.0,
            cold_answers: vec!["42".into(), "47".into()],
            hot_answers: vec!["47".into(), "73".into()],
            cold_distinct: 2,
            hot_distinct: 2,
            verdict: Verdict::Ignored,
            explanation: "пояснение".into(),
        });
        let md = to_markdown(&c);
        assert!(md.contains("параметр игнорируется"));
        assert!(md.find("параметр игнорируется") < md.find("## Ответы"));
    }

    /// Сырой вывод модели не должен ломать таблицу пробы.
    #[test]
    fn probe_answers_are_escaped() {
        let cell = probe_cell(&["**73**".into(), "a|b".into()]);
        assert_eq!(cell, "`**73**`, `a\\|b`");
    }
}
