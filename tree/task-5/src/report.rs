//! Выгрузка сравнения в markdown — это и есть сдаваемый артефакт задания:
//! таблица замеров (время, токены, стоимость), ответы, разбор судьи и ссылки.

use crate::api::Billing;
use crate::ladder::{Ladder, Rung};
use crate::verify::{LadderCheck, Verdict};

pub fn to_markdown(l: &Ladder) -> String {
    let mut out = String::new();
    out.push_str("# Один запрос на трёх моделях: слабая → средняя → сильная\n\n");
    out.push_str(&format!(
        "- **Запрос:** {}\n- **Судья:** `{}`\n\
         - **Одинаково для всех ступеней:** `temperature = {}`, `max_tokens = {}`, \
         один и тот же system-промпт\n- **Общее время (ступени шли параллельно):** {:.1} с\n\n",
        l.prompt, l.judge_model, l.temperature, l.max_tokens,
        l.total_ms as f64 / 1000.0
    ));

    if let Some(check) = &l.check {
        out.push_str(&check_section(check));
    }

    out.push_str("## Замеры\n\n");
    out.push_str(
        "| ступень | модель | время | токенов вход | токенов выход | из них рассуждения \
         | ток./с | стоимость | оценка судьи |\n|---|---|---|---|---|---|---|---|---|\n",
    );
    for r in &l.rungs {
        let m = &r.measure;
        out.push_str(&format!(
            "| {} | `{}`{} | {:.1} с | {} | {} | {} | {:.0} | {} | {} |\n",
            r.label,
            r.model,
            match r.effort {
                Some(e) => format!(" ({})", e.as_str()),
                None => String::new(),
            },
            m.latency_ms as f64 / 1000.0,
            m.prompt_tokens,
            m.completion_tokens,
            m.reasoning_tokens,
            m.tokens_per_sec,
            cost_cell(r),
            match r.score {
                Some(s) => format!("{s:.0}/10"),
                None => "—".into(),
            }
        ));
    }
    out.push_str(
        "\nСтоимость — по публичному прайсу модели за фактические токены. \
         «Подписка» значит, что этот конкретный маршрут отдельно за токены \
         не тарифицируется: цена рядом показывает, во сколько тот же объём \
         обошёлся бы при оплате по токенам.\n\n",
    );

    if let Some(line) = cost_cross_check(l) {
        out.push_str(&line);
    }

    out.push_str("## Ответы\n");
    for r in &l.rungs {
        out.push_str(&format!(
            "\n### {} — `{}`\n\n<sub>{} ток., {:.1} с, finish_reason: `{}`, \
             слов: {}, уникальных: {:.0}%</sub>\n\n",
            r.label,
            r.model,
            r.measure.completion_tokens,
            r.measure.latency_ms as f64 / 1000.0,
            r.finish_reason,
            r.measure.words,
            r.measure.distinct_ratio * 100.0
        ));
        out.push_str(&format!("> {}\n\n", r.answer.replace('\n', "\n> ")));
    }

    out.push_str(&format!(
        "## Разбор ({})\n\nСудья получил три ответа вслепую — без имён моделей \
         и без замеров, только тексты под номерами 1/2/3 в порядке \
         слабая → средняя → сильная.\n\n{}\n\n",
        l.judge_model, l.summary
    ));

    out.push_str(&links_section(l));
    out
}

/// Ставится в начало отчёта: без неё непонятно, можно ли вообще верить,
/// что цифры относятся к заявленным моделям.
fn check_section(c: &LadderCheck) -> String {
    let (label, note) = match c.verdict {
        Verdict::Confirmed => (
            "✅ лестница подтверждена",
            "Замеры ниже относятся к заявленным моделям, и ступени различимы.",
        ),
        Verdict::Substituted => (
            "❌ площадка ответила другой моделью",
            "**Замеры ниже относятся не к той модели, которая заявлена** — сравнивать по ним нельзя.",
        ),
        Verdict::Flat => (
            "⚠️ проба не различила ступени",
            "Замеры ниже относятся к заявленным моделям, но порядок ступеней этой пробой не установлен.",
        ),
        Verdict::Inverted => (
            "⚠️ порядок ступеней не подтверждён",
            "Слабая ступень решила пробу лучше сильной.",
        ),
    };

    let mut out = format!(
        "## Проверка: это действительно лестница?\n\n**{label}**\n\n\
         Каждой ступени заданы {} коротких вопроса с заранее известным ответом \
         при `temperature = 0` — проверяется не «тексты разные», а попадание \
         в число. Плюс сверяется, ту ли модель назвала площадка в ответе.\n\n",
        c.questions.len()
    );
    out.push_str("| вопрос | верный ответ |\n|---|---|\n");
    for (q, e) in c.questions.iter().zip(&c.expected) {
        out.push_str(&format!("| {q} | `{e}` |\n"));
    }
    out.push_str("\n| ступень | модель ответила | решено | ответы |\n|---|---|---|---|\n");
    for r in &c.rungs {
        out.push_str(&format!(
            "| {} | {} `{}` | {}/{} | {} |\n",
            r.label,
            if r.model_matches { "✅" } else { "❌" },
            r.served_model,
            r.score,
            c.questions.len(),
            r.answers
                .iter()
                .zip(&r.correct)
                .map(|(a, ok)| format!(
                    "{}`{}`",
                    if *ok { "✅ " } else { "❌ " },
                    a.replace('`', "'").replace('|', "\\|").replace('\n', " ")
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    out.push_str(&format!("\n{}\n\n{note}\n\n", c.explanation));
    out
}

fn cost_cell(r: &Rung) -> String {
    let usd = format!("${:.5}", r.measure.cost_usd);
    match r.billing {
        Billing::PerToken => usd,
        Billing::Subscription => format!("подписка (по прайсу {usd})"),
    }
}

/// Единственная площадка, которая сообщает реально списанную сумму, — это
/// проверка того, что колонка «стоимость» не выдумана: считаем сами и
/// сравниваем со счётом.
fn cost_cross_check(l: &Ladder) -> Option<String> {
    let r = l.rungs.iter().find(|r| r.measure.billed_usd.is_some())?;
    let billed = r.measure.billed_usd?;
    let ours = r.measure.cost_usd;
    let delta = if billed == 0.0 {
        0.0
    } else {
        (ours - billed).abs() / billed * 100.0
    };
    Some(format!(
        "**Сверка цены.** `{}` — единственный маршрут, который возвращает \
         фактически списанную сумму: ${billed:.5} против ${ours:.5}, посчитанных \
         здесь по прайсу (расхождение {delta:.1}%). Значит колонка «стоимость» \
         не оценка, а проверяемая величина.\n\n",
        r.model
    ))
}

fn links_section(l: &Ladder) -> String {
    let mut out = String::from("## Ссылки\n\n");
    for r in &l.rungs {
        out.push_str(&format!(
            "- **{}** — [{}]({}) · [прайс]({}) · площадка: `{}`\n",
            r.label, r.model, r.model_url, r.price_url, r.provider
        ));
    }
    out.push_str(&format!(
        "- **судья** — [{}]({}) · площадка: `synthetic`\n",
        l.judge_model, l.judge_url
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Effort;
    use crate::ladder::{Measure, Tier};
    use crate::verify::RungCheck;

    fn rung(label: &str, billing: Billing, billed: Option<f64>) -> Rung {
        Rung {
            tier: Tier::Strong,
            label: label.into(),
            provider: "openrouter".into(),
            model: "anthropic/claude-opus-5".into(),
            served_model: "anthropic/claude-opus-5".into(),
            effort: Some(Effort::High),
            billing,
            model_url: "https://openrouter.ai/anthropic/claude-opus-5".into(),
            price_url: "https://www.anthropic.com/pricing".into(),
            answer: "первая строка\nвторая строка".into(),
            finish_reason: "stop".into(),
            measure: Measure {
                latency_ms: 4000,
                prompt_tokens: 40,
                completion_tokens: 200,
                reasoning_tokens: 120,
                tokens_per_sec: 50.0,
                cost_usd: 0.0052,
                billed_usd: billed,
                chars: 27,
                words: 4,
                distinct_ratio: 0.5,
            },
            score: Some(9.0),
        }
    }

    fn ladder(billed: Option<f64>) -> Ladder {
        Ladder {
            prompt: "тест".into(),
            judge_model: "hf:moonshotai/Kimi-K3".into(),
            judge_url: "https://huggingface.co/moonshotai/Kimi-K3".into(),
            temperature: 0.3,
            max_tokens: 1200,
            rungs: vec![rung("сильная", Billing::PerToken, billed)],
            summary: "вывод".into(),
            check: None,
            total_ms: 4200,
        }
    }

    #[test]
    fn multiline_answers_stay_inside_the_blockquote() {
        let md = to_markdown(&ladder(None));
        assert!(md.contains("> первая строка\n> вторая строка"));
    }

    #[test]
    fn the_measurements_table_carries_all_three_required_metrics() {
        let md = to_markdown(&ladder(None));
        assert!(md.contains("| сильная | `anthropic/claude-opus-5` (high) | 4.0 с | 40 | 200 | 120 | 50 | $0.00520 | 9/10 |"));
    }

    /// Задание требует ссылки — их отсутствие это неполный отчёт, а не мелочь.
    #[test]
    fn links_section_lists_every_model_including_the_judge() {
        let md = to_markdown(&ladder(None));
        assert!(md.contains("https://openrouter.ai/anthropic/claude-opus-5"));
        assert!(md.contains("https://www.anthropic.com/pricing"));
        assert!(md.contains("https://huggingface.co/moonshotai/Kimi-K3"));
    }

    /// Бесплатный для нас маршрут не должен выглядеть как «модель ничего
    /// не стоит»: прайс обязан остаться на виду.
    #[test]
    fn a_subscription_route_still_shows_the_list_price() {
        let cell = cost_cell(&rung("слабая", Billing::Subscription, None));
        assert_eq!(cell, "подписка (по прайсу $0.00520)");
    }

    #[test]
    fn cross_check_appears_only_when_the_platform_reports_a_bill() {
        assert!(cost_cross_check(&ladder(None)).is_none());
        let line = cost_cross_check(&ladder(Some(0.0052))).unwrap();
        assert!(line.contains("$0.00520"));
        assert!(line.contains("расхождение 0.0%"));
    }

    #[test]
    fn no_check_means_no_verification_section() {
        assert!(!to_markdown(&ladder(None)).contains("это действительно лестница"));
    }

    /// Самое важное свойство отчёта: если площадка подсунула другую модель,
    /// читатель обязан узнать это раньше, чем увидит таблицу замеров.
    #[test]
    fn substitution_warns_before_the_numbers() {
        let mut l = ladder(None);
        l.check = Some(LadderCheck {
            questions: vec!["сколько будет 2+2?".into()],
            expected: vec!["4".into()],
            rungs: vec![RungCheck {
                tier: Tier::Strong,
                label: "сильная".into(),
                model: "anthropic/claude-opus-5".into(),
                served_model: "anthropic/claude-haiku-4.5".into(),
                model_matches: false,
                answers: vec!["4".into()],
                correct: vec![true],
                score: 1,
                probe_ms: 100,
                probe_tokens: 10,
            }],
            verdict: Verdict::Substituted,
            explanation: "пояснение".into(),
        });
        let md = to_markdown(&l);
        assert!(md.contains("ответила другой моделью"));
        assert!(md.find("ответила другой моделью") < md.find("## Замеры"));
    }

    /// Сырой вывод модели не должен ломать таблицу пробы.
    #[test]
    fn probe_answers_are_escaped_in_the_table() {
        let mut l = ladder(None);
        l.check = Some(LadderCheck {
            questions: vec!["q".into()],
            expected: vec!["3".into()],
            rungs: vec![RungCheck {
                tier: Tier::Weak,
                label: "слабая".into(),
                model: "qwen3.8-27b".into(),
                served_model: "qwen3.8-27b".into(),
                model_matches: true,
                answers: vec!["a|b\nc `d`".into()],
                correct: vec![false],
                score: 0,
                probe_ms: 100,
                probe_tokens: 10,
            }],
            verdict: Verdict::Flat,
            explanation: "пояснение".into(),
        });
        let md = to_markdown(&l);
        assert!(md.contains("❌ `a\\|b c 'd'`"));
    }
}
